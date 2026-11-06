//! Desktop session indicator: context usage inside, independent agent state outside.
//! The phone's plain context ring lives elsewhere. This component has no button
//! behaviour; its caller owns the hit target, tooltip and session-sheet action.

use leptos::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Waiting,
    Failed,
}

impl AgentState {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::Waiting => "Waiting on you",
            Self::Failed => "Last turn failed",
        }
    }

    fn rim_class(self) -> &'static str {
        match self {
            Self::Idle => "stroke-zinc-400 dark:stroke-zinc-500",
            Self::Working => "stroke-blue-500 dark:stroke-blue-400",
            Self::Waiting => "stroke-amber-500 dark:stroke-amber-400",
            Self::Failed => "stroke-red-500 dark:stroke-red-400",
        }
    }
}

/// Words shared by the SVG's accessible name and a caller's button label or
/// tooltip. Preserve the actual context percentage, even above 100; only the
/// drawn arc is clamped. An unknown context is not described as zero usage.
pub fn description(state: AgentState, context: Option<u64>, background: usize) -> String {
    let context = match context {
        Some(percent) => format!("Context {percent} percent used."),
        None => "Context usage unavailable.".to_owned(),
    };
    let background = match background {
        0 => "No background tasks running.".to_owned(),
        1 => "1 background task running.".to_owned(),
        count => format!("{count} background tasks running."),
    };
    format!("{}. {context} {background}", state.label())
}

fn context_arc(percent: u64) -> String {
    // pathLength normalizes the circumference to 100 SVG units.
    format!("{} 100", percent.min(100))
}

/// A 24px desktop ring with a positive-only background
/// count badge. All three inputs are independent and reactive; plain values
/// work too via `#[prop(into)]`. The badge may extend outside the 24px box.
///
/// A wrapper button can use [`description`] for its own accessible name/tooltip.
/// The SVG also names itself, so the visual remains accessible on its own.
#[component]
pub fn SessionRing(
    #[prop(into)] state: Signal<AgentState>,
    #[prop(into)] context: Signal<Option<u64>>,
    #[prop(into)] background: Signal<usize>,
) -> impl IntoView {
    let label = move || description(state.get(), context.get(), background.get());
    let count = Memo::new(move |last: Option<&usize>| if background.get() > 0 { background.get() } else { last.copied().unwrap_or(0) });
    view! {
        <span class="relative inline-flex size-6 shrink-0 align-middle">
            // Rotate the whole SVG on its own layer, never an SVG child or
            // the context arc. Retain both drawings across state updates.
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" aria-hidden="true"
                class=move || if state.get() == AgentState::Working { "absolute inset-0 size-6 c6-ring-working" } else { "absolute inset-0 size-6" }>
                // A full outer rim; waiting is dashed, failed has a drawn cross.
                <circle cx="12" cy="12" r="10.5" stroke-width="1.5"
                    class=move || state.get().rim_class()
                    stroke-dasharray=move || if state.get() == AgentState::Waiting { "2 2" } else { "none" } />
            </svg>
            <svg width="24" height="24" viewBox="0 0 24 24" class="size-6 shrink-0" fill="none"
                role="img" aria-label=label>
                <title>{label}</title>
                <circle cx="12" cy="12" r="7" stroke-width="2"
                    class="stroke-zinc-200 dark:stroke-zinc-700" />
                {move || context.get().map(|percent| view! {
                    <circle cx="12" cy="12" r="7" stroke-width="2" pathLength="100"
                        class="stroke-zinc-600 dark:stroke-zinc-300"
                        stroke-dasharray=context_arc(percent) transform="rotate(-90 12 12)" />
                })}
                {move || (state.get() == AgentState::Failed).then(|| view! {
                    <path d="m10 10 4 4m0-4-4 4" stroke-width="1.5" stroke-linecap="round"
                        class="stroke-red-500 dark:stroke-red-400" />
                })}
            </svg>
            // The SVG's name includes this count; do not announce it twice.
            <span class="c6-presence absolute -right-1.5 -top-1" data-open=move || (background.get() > 0).to_string()>
                <span aria-hidden="true"
                    class="c6-ring-badge flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-zinc-700 px-0.5 text-[9px] font-medium leading-none text-white dark:bg-zinc-300 dark:text-zinc-900">
                    <crate::util::MotionCount value=Signal::derive(move || count.get()) />
                </span>
            </span>
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_keep_state_context_and_background_separate() {
        for (state, label) in [
            (AgentState::Idle, "Idle"),
            (AgentState::Working, "Working"),
            (AgentState::Waiting, "Waiting on you"),
            (AgentState::Failed, "Last turn failed"),
        ] {
            assert_eq!(description(state, Some(137), 2), format!("{label}. Context 137 percent used. 2 background tasks running."));
        }
        assert_eq!(description(AgentState::Idle, None, 0), "Idle. Context usage unavailable. No background tasks running.");
        assert_eq!(description(AgentState::Working, Some(0), 1), "Working. Context 0 percent used. 1 background task running.");
    }

    #[test]
    fn arc_clamps_without_overflow() {
        for (percent, expected) in [(0, "0 100"), (42, "42 100"), (100, "100 100"), (137, "100 100"), (u64::MAX, "100 100")] {
            assert_eq!(context_arc(percent), expected);
        }
        assert!(description(AgentState::Idle, Some(u64::MAX), 0).contains(&u64::MAX.to_string()));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render(state: AgentState, context: Option<u64>, background: usize) -> String {
        let owner = Owner::new();
        owner.with(|| untrack(|| view! { <SessionRing state=state context=context background=background /> }.to_html()))
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn svg_has_a_worded_name_and_fixed_size() {
        let html = render(AgentState::Working, Some(137), 2);
        let label = description(AgentState::Working, Some(137), 2);
        for expected in [
            "width=\"24\"",
            "height=\"24\"",
            "viewBox=\"0 0 24 24\"",
            "role=\"img\"",
            &format!("aria-label=\"{label}\""),
            &format!("<title>{label}</title>"),
        ] {
            assert!(html.contains(expected), "{expected} missing: {html}");
        }
        assert!(!html.contains("<button"));
        assert!(!html.contains("animate-"));
        assert!(!html.contains("transition-"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn context_track_is_neutral_and_unknown_is_not_zero() {
        for context in [None, Some(0), Some(42), Some(100), Some(137), Some(u64::MAX)] {
            let html = render(AgentState::Idle, context, 0);
            assert!(html.contains("stroke-zinc-200 dark:stroke-zinc-700"));
            assert_eq!(html.matches("<circle").count(), if context.is_some() { 3 } else { 2 });
            if let Some(percent) = context {
                assert!(html.contains("stroke-zinc-600 dark:stroke-zinc-300"));
                assert!(html.contains(&format!("stroke-dasharray=\"{}\"", context_arc(percent))));
                assert!(html.contains(&format!("Context {percent} percent used.")));
                assert!(html.contains("pathLength=\"100\""));
                assert!(html.contains("transform=\"rotate(-90 12 12)\""));
            } else {
                assert!(html.contains("Context usage unavailable."));
                assert!(!html.contains("pathLength="));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn all_four_rims_have_independent_state_and_noncolor_attention_cues() {
        for (state, color) in [
            (AgentState::Idle, "stroke-zinc-400"),
            (AgentState::Working, "stroke-blue-500"),
            (AgentState::Waiting, "stroke-amber-500"),
            (AgentState::Failed, "stroke-red-500"),
        ] {
            let html = render(state, Some(42), 0);
            let rim = html.split("<circle").nth(1).unwrap().split('>').next().unwrap();
            assert!(rim.contains("r=\"10.5\""));
            assert!(rim.contains(color));
            assert!(rim.contains(if state == AgentState::Waiting { "stroke-dasharray=\"2 2\"" } else { "stroke-dasharray=\"none\"" }));
            assert_eq!(html.contains("<path"), state == AgentState::Failed);
            if state == AgentState::Failed {
                assert!(html.contains("d=\"m10 10 4 4m0-4-4 4\""));
            }
            // Agent state must never change the neutral context arc.
            assert!(html.contains("stroke-dasharray=\"42 100\""));
            assert!(html.contains("stroke-zinc-600 dark:stroke-zinc-300"));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn only_the_outer_svg_spins_and_the_context_keeps_its_accessible_name() {
        let html = render(AgentState::Working, Some(42), 0);
        let rim_svg = html.split("<svg").nth(1).unwrap().split("</svg>").next().unwrap();
        let context_svg = html.split("<svg").nth(2).unwrap().split("</svg>").next().unwrap();
        assert!(rim_svg.contains("aria-hidden=\"true\""));
        assert!(rim_svg.split('>').next().unwrap().contains("c6-ring-working"));
        assert!(!rim_svg.contains("r=\"7\""));
        assert!(!context_svg.contains("c6-ring-working"));
        assert!(context_svg.contains("role=\"img\""));
        assert!(context_svg.contains("stroke-dasharray=\"42 100\""));
        let css = include_str!("../style/input.css");
        assert!(!css.contains("c6-ring-arc"));
        assert!(css.contains(".c6-ring-working > circle { stroke-dasharray: 20 46; }"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn retained_badge_is_only_visible_for_positive_background_counts() {
        for background in [0, 1, 12, usize::MAX] {
            let html = render(AgentState::Working, None, background);
            assert!(html.contains("aria-hidden=\"true\""));
            assert!(html.contains(if background > 0 { "data-open=\"true\"" } else { "data-open=\"false\"" }));
            if background > 0 {
                assert!(html.contains(&format!(">{background}</span>")), "{html}");
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn signal_inputs_are_read_when_rendered() {
        let owner = Owner::new();
        owner.with(|| {
            untrack(|| {
                let state = RwSignal::new(AgentState::Idle);
                let context = RwSignal::new(None::<u64>);
                let background = RwSignal::new(0_usize);
                let html = view! { <SessionRing state=state context=context background=background /> };
                state.set(AgentState::Failed);
                context.set(Some(137));
                background.set(3);
                let html = html.to_html();
                assert!(html.contains(&description(AgentState::Failed, Some(137), 3)));
                assert!(html.contains("stroke-red-500"));
                assert!(html.contains("stroke-dasharray=\"100 100\""));
                assert!(html.contains(">3</span>"));
            });
        });
    }
}
