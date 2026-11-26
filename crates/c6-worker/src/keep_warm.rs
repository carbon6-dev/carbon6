//! Pure keep-warm policy. The actor supplies its existing background registry,
//! call clock and original wait clock; a ping must not reset the latter.

use c6_api::{KeepWarmSettings, Price};
use c6_llm::Usage;
use c6_tools::TaskInfo;

const MIN_INTERVAL_MS: u64 = 60_000;
const MAX_WAIT_MS: u64 = 12 * 60 * 60 * 1_000;
const DEFAULT_REMAINING_SECONDS: u64 = 90 * 60;

/// Monotonic time remains injectable, but may stop during machine sleep.
/// Either clock expiring must prevent a new maintenance request.
#[derive(Clone, Copy)]
pub(crate) struct CacheDeadline {
    pub instant: tokio::time::Instant,
    wall_ms: i64,
}

impl CacheDeadline {
    pub(crate) fn new(instant: tokio::time::Instant) -> Self {
        let remaining = instant.saturating_duration_since(tokio::time::Instant::now()).as_millis().min(i64::MAX as u128) as i64;
        Self { instant, wall_ms: c6_core::now_ms().saturating_add(remaining) }
    }

    pub(crate) fn expired(self) -> bool {
        tokio::time::Instant::now() >= self.instant || c6_core::now_ms() >= self.wall_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitingOn {
    User,
    Background,
    Nothing,
}

/// An active turn isn't a wait. Native Claude Code work is supplied from its
/// existing process state, not inferred from a second task registry.
pub(crate) fn waiting_on(tasks: &[TaskInfo], native_background_live: bool, turn_active: bool) -> WaitingOn {
    if turn_active {
        WaitingOn::Nothing
    } else if native_background_live || tasks.iter().any(|t| t.running && crate::subagent::counted_background(t.notify, t.kind.as_str())) {
        WaitingOn::Background
    } else {
        WaitingOn::User
    }
}

/// Providers normalize these into disjoint counts: OpenAI subtracts cached
/// input, while Anthropic and Claude Code already report uncached input.
/// Output and reasoning aren't additional prompt tokens.
pub(crate) fn context_tokens(usage: &Usage) -> u64 {
    usage.input_tokens.saturating_add(usage.cache_read_tokens).saturating_add(usage.cache_write_tokens)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheEconomics {
    /// Cache-read price divided by the cost of rewriting the same prefix.
    pub read_to_rewrite_ratio: f64,
    /// A real cache read was observed on this connection/model. In particular,
    /// a compatible endpoint's claimed protocol alone isn't cache evidence.
    pub cache_observed: bool,
}

impl CacheEconomics {
    /// Missing cache pricing is not evidence of a discount. Use the cheaper
    /// possible rewrite price to avoid overstating the savings.
    pub(crate) fn from_price(price: &Price, cache_observed: bool) -> Option<Self> {
        let read = price.cache_read?;
        let write = price.cache_write.unwrap_or(price.input);
        if ![read, write, price.input].into_iter().all(|v| v.is_finite() && v >= 0.0) {
            return None;
        }
        let rewrite = write.min(price.input);
        (rewrite > 0.0).then_some(Self { read_to_rewrite_ratio: read / rewrite, cache_observed })
    }
}

pub(crate) struct KeepWarmInput<'a> {
    pub connection_id: &'a str,
    pub waiting_on: WaitingOn,
    pub context_tokens: u64,
    pub lifetime_seconds: Option<u64>,
    pub last_call_ms: u64,
    /// Start of the original background wait, preserved across warm calls.
    pub wait_start_ms: u64,
    pub now_ms: u64,
    /// Successful warm calls during this original wait (not real work calls).
    pub pings_sent: u64,
    /// Mean of a simple exponential remaining-work prior; None uses 90 minutes.
    pub projected_remaining_seconds: Option<u64>,
    pub economics: Option<CacheEconomics>,
}

/// Absolute next ping time, or None if ineligible. A due ping returns now_ms.
///
/// This never schedules a cold ping: the previous prefix must still be warm.
/// Budget includes *all* prior pings, projected future pings and one resume
/// read, and must be strictly less than half the expected avoided rewrite.
/// An exponential remaining-work prior estimates the probability that live
/// work outlasts the warm prefix's remaining life at the ping deadline.
pub(crate) fn schedule(settings: &KeepWarmSettings, input: &KeepWarmInput<'_>) -> Option<u64> {
    if !settings.enabled
        || input.waiting_on != WaitingOn::Background
        || input.context_tokens == 0
        || input.context_tokens < settings.threshold_tokens
        || settings.disabled_connections.iter().any(|id| id == input.connection_id)
        || input.wait_start_ms > input.now_ms
        || input.last_call_ms > input.now_ms
    {
        return None;
    }
    let ttl = input.lifetime_seconds?;
    // At most a minute of cache life cannot support both the interval and
    // an early ping. Saturate large external values rather than wrapping.
    if ttl <= 60 {
        return None;
    }
    let ttl_ms = ttl.saturating_mul(1_000);
    let expires_ms = input.last_call_ms.saturating_add(ttl_ms);
    if input.now_ms >= expires_ms {
        return None;
    }
    let margin_ms = u64::from(settings.margin_seconds).min(ttl / 5).saturating_mul(1_000);
    let interval_ms = ttl_ms.saturating_sub(margin_ms).max(MIN_INTERVAL_MS);
    let due_ms = input.last_call_ms.saturating_add(interval_ms).max(input.now_ms);
    let cap_ms = u64::from(settings.cap_seconds).saturating_mul(1_000).min(MAX_WAIT_MS);
    let stop_ms = input.wait_start_ms.saturating_add(cap_ms);
    if due_ms >= stop_ms || due_ms >= expires_ms {
        return None;
    }

    let economics = input.economics?;
    let ratio = economics.read_to_rewrite_ratio;
    if !economics.cache_observed || !ratio.is_finite() || ratio < 0.0 {
        return None;
    }
    let remaining_ms = input.projected_remaining_seconds.unwrap_or(DEFAULT_REMAINING_SECONDS).saturating_mul(1_000);
    if remaining_ms == 0 {
        return None;
    }
    // Include this ping even if the supplied projection is shorter than an
    // interval. Do not truncate projections at the cap: after it we may still
    // need a cold rewrite, so that isn't a justification for spending more.
    let future_pings = remaining_ms.div_ceil(interval_ms).max(1);
    let reads = input.pings_sent.saturating_add(future_pings).saturating_add(1);
    // Condition on work still running when the ping is due. The memoryless
    // exponential prior is deliberately simple, not a measured duration model.
    // Finishing before expiry wouldn't require a rewrite, so discount the
    // benefit by the probability of surviving the remaining cache margin.
    let survival_probability = (-((expires_ms - due_ms) as f64) / remaining_ms as f64).exp();
    if reads as f64 * ratio >= 0.5 * survival_probability {
        return None;
    }
    Some(due_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_tools::TaskKind;

    #[tokio::test(start_paused = true)]
    async fn keep_warm_dispatch_deadline_obeys_both_clocks() {
        let deadline = CacheDeadline::new(tokio::time::Instant::now() + std::time::Duration::from_secs(60));
        assert!(!deadline.expired());
        let slept = CacheDeadline { wall_ms: c6_core::now_ms() - 1, ..deadline };
        assert!(slept.expired(), "machine sleep cannot extend the dispatch deadline");
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert!(deadline.expired(), "the injected monotonic clock also expires the request");
    }

    fn settings() -> KeepWarmSettings {
        KeepWarmSettings {
            enabled: true,
            threshold_tokens: 50_000,
            margin_seconds: 300,
            cap_seconds: 12 * 60 * 60,
            disabled_connections: vec![],
        }
    }

    fn input(ttl: u64) -> KeepWarmInput<'static> {
        KeepWarmInput {
            connection_id: "connection",
            waiting_on: WaitingOn::Background,
            context_tokens: 50_000,
            lifetime_seconds: Some(ttl),
            last_call_ms: 0,
            wait_start_ms: 0,
            now_ms: 0,
            pings_sent: 0,
            projected_remaining_seconds: None,
            economics: Some(CacheEconomics { read_to_rewrite_ratio: 0.01, cache_observed: true }),
        }
    }

    #[test]
    fn five_and_sixty_minute_caches() {
        assert_eq!(schedule(&settings(), &input(300)), Some(240_000));
        let mut i = input(3_600);
        i.economics.as_mut().unwrap().read_to_rewrite_ratio = 0.1;
        assert_eq!(schedule(&settings(), &i), Some(55 * 60_000));
        i.now_ms = 55 * 60_000;
        assert_eq!(schedule(&settings(), &i), Some(i.now_ms));
        i.now_ms += 1;
        assert_eq!(schedule(&settings(), &i), Some(i.now_ms));
        i.last_call_ms = i.now_ms;
        i.pings_sent += 1;
        assert_eq!(schedule(&settings(), &i), Some(i.last_call_ms + 55 * 60_000));
    }

    #[test]
    fn minimum_interval_and_cold_cache() {
        let mut i = input(61);
        i.projected_remaining_seconds = Some(60);
        assert_eq!(schedule(&settings(), &i), Some(60_000));
        for ttl in [0, 1, 59, 60] {
            i.lifetime_seconds = Some(ttl);
            assert_eq!(schedule(&settings(), &i), None);
        }
        i.lifetime_seconds = None;
        assert_eq!(schedule(&settings(), &i), None);
        i.lifetime_seconds = Some(300);
        i.now_ms = 300_000;
        assert_eq!(schedule(&settings(), &i), None);
        let mut s = settings();
        s.margin_seconds = 0;
        assert_eq!(schedule(&s, &input(3_600)), None, "no safe before-expiry ping with zero margin");
    }

    #[test]
    fn original_wait_cap_survives_pings_and_cannot_exceed_twelve_hours() {
        let mut i = input(3_600);
        i.last_call_ms = 11 * 60 * 60_000;
        i.now_ms = i.last_call_ms;
        let mut s = settings();
        s.cap_seconds = u32::MAX;
        assert_eq!(schedule(&s, &i), Some(i.last_call_ms + 55 * 60_000));
        i.last_call_ms += 55 * 60_000;
        i.now_ms = i.last_call_ms;
        assert_eq!(schedule(&s, &i), None);
        s.cap_seconds = 240;
        assert_eq!(schedule(&s, &input(300)), None, "don't ping at the cap");
    }

    #[test]
    fn eligibility() {
        let mut i = input(3_600);
        i.context_tokens -= 1;
        assert_eq!(schedule(&settings(), &i), None);
        i.context_tokens += 1;
        for waiting in [WaitingOn::User, WaitingOn::Nothing] {
            i.waiting_on = waiting;
            assert_eq!(schedule(&settings(), &i), None);
        }
        i.waiting_on = WaitingOn::Background;
        let mut s = settings();
        s.enabled = false;
        assert_eq!(schedule(&s, &i), None);
        s.enabled = true;
        s.disabled_connections.push("connection".into());
        assert_eq!(schedule(&s, &i), None);
        i.wait_start_ms = 1;
        assert_eq!(schedule(&settings(), &i), None);
        i.wait_start_ms = 0;
        i.last_call_ms = 1;
        assert_eq!(schedule(&settings(), &i), None);
    }

    #[test]
    fn economics_require_observed_discount_and_total_budget() {
        let mut i = input(300);
        i.economics.as_mut().unwrap().read_to_rewrite_ratio = 0.1;
        assert_eq!(schedule(&settings(), &i), None, "23 projected pings aren't worth a rewrite");
        i.lifetime_seconds = Some(3_600);
        assert!(schedule(&settings(), &i).is_some());
        i.pings_sent = 2;
        assert_eq!(schedule(&settings(), &i), None, "two previous + two future + resume reaches half");
        i.pings_sent = 0;
        for ratio in [0.5, 1.0, -0.1, f64::NAN, f64::INFINITY] {
            i.economics.as_mut().unwrap().read_to_rewrite_ratio = ratio;
            assert_eq!(schedule(&settings(), &i), None);
        }
        i.economics = Some(CacheEconomics { read_to_rewrite_ratio: 0.01, cache_observed: false });
        assert_eq!(schedule(&settings(), &i), None, "compatible protocol is not cache evidence");
        i.economics = None;
        assert_eq!(schedule(&settings(), &i), None);
    }

    #[test]
    fn conservative_price_ratio() {
        let mut p = Price { model: "model".into(), input: 10.0, output: 50.0, cache_read: Some(1.0), cache_write: Some(12.5) };
        assert_eq!(CacheEconomics::from_price(&p, true).unwrap().read_to_rewrite_ratio, 0.1);
        p.cache_write = Some(5.0);
        assert_eq!(CacheEconomics::from_price(&p, true).unwrap().read_to_rewrite_ratio, 0.2);
        p.cache_read = None;
        assert!(CacheEconomics::from_price(&p, true).is_none());
        p.cache_read = Some(f64::NAN);
        assert!(CacheEconomics::from_price(&p, true).is_none());
        p.cache_read = Some(1.0);
        p.input = 0.0;
        assert!(CacheEconomics::from_price(&p, true).is_none());
    }

    #[test]
    fn economics_discount_rewrite_benefit_by_survival_probability() {
        let mut i = input(3_600);
        i.now_ms = 55 * 60_000;
        i.economics.as_mut().unwrap().read_to_rewrite_ratio = 0.1;
        assert_eq!(schedule(&settings(), &i), Some(i.now_ms), "90-minute prior survives a five-minute margin with p=.946");
        i.projected_remaining_seconds = Some(60);
        assert_eq!(schedule(&settings(), &i), None, "one-minute prior has only p=.0067 of needing a rewrite");
        i.projected_remaining_seconds = Some(300);
        assert_eq!(schedule(&settings(), &i), None, "two reads cost .2 but half the expected rewrite is only .184");
        i.now_ms = 59 * 60_000;
        assert_eq!(schedule(&settings(), &i), Some(i.now_ms), "late but warm: one minute of cache remains, p=.819");
        i.projected_remaining_seconds = Some(0);
        assert_eq!(schedule(&settings(), &i), None);
    }

    #[test]
    fn shared_background_classification() {
        let task = |kind, running, notify| TaskInfo { id: "id".into(), kind, description: String::new(), running, notify };
        assert_eq!(waiting_on(&[], false, false), WaitingOn::User);
        assert_eq!(waiting_on(&[], true, false), WaitingOn::Background);
        assert_eq!(waiting_on(&[], true, true), WaitingOn::Nothing);
        assert_eq!(waiting_on(&[task(TaskKind::Shell, true, false)], false, false), WaitingOn::User);
        assert_eq!(waiting_on(&[task(TaskKind::Shell, false, true)], false, false), WaitingOn::User);
        for kind in [TaskKind::Shell, TaskKind::Agent, TaskKind::ClaudeCode] {
            assert_eq!(waiting_on(&[task(kind, true, true)], false, false), WaitingOn::Background);
        }
        assert_eq!(waiting_on(&[task(TaskKind::ClaudeCode, true, false)], false, false), WaitingOn::Background);
    }

    #[test]
    fn normalized_context_is_disjoint_and_saturates() {
        let usage = Usage { input_tokens: 10, cache_read_tokens: 30, cache_write_tokens: 20, output_tokens: 100, reasoning_tokens: 50 };
        assert_eq!(context_tokens(&usage), 60);
        assert_eq!(context_tokens(&Usage { input_tokens: u64::MAX, ..usage }), u64::MAX);
    }
}
