//! A message's text as Slack shows it (docs/chat.md §3): code blocks,
//! quotes, `code`, *bold*, _italic_, ~strike~, links, :emoji:, @mentions
//! (yours stand out) and #channels.

use leptos::prelude::*;

/// Common emoji by their Slack names (custom ones show as `:name:`).
pub const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("thumbsup", "👍"),
    ("-1", "👎"),
    ("thumbsdown", "👎"),
    ("white_check_mark", "✅"),
    ("heavy_check_mark", "✔️"),
    ("x", "❌"),
    ("eyes", "👀"),
    ("tada", "🎉"),
    ("heart", "❤️"),
    ("joy", "😂"),
    ("smile", "😄"),
    ("slightly_smiling_face", "🙂"),
    ("grinning", "😀"),
    ("laughing", "😆"),
    ("rolling_on_the_floor_laughing", "🤣"),
    ("wink", "😉"),
    ("blush", "😊"),
    ("sweat_smile", "😅"),
    ("thinking_face", "🤔"),
    ("raised_hands", "🙌"),
    ("clap", "👏"),
    ("pray", "🙏"),
    ("muscle", "💪"),
    ("fire", "🔥"),
    ("rocket", "🚀"),
    ("100", "💯"),
    ("star", "⭐"),
    ("sparkles", "✨"),
    ("zap", "⚡"),
    ("boom", "💥"),
    ("wave", "👋"),
    ("ok_hand", "👌"),
    ("point_up", "☝️"),
    ("point_right", "👉"),
    ("point_left", "👈"),
    ("point_down", "👇"),
    ("raised_hand", "✋"),
    ("v", "✌️"),
    ("crossed_fingers", "🤞"),
    ("handshake", "🤝"),
    ("facepalm", "🤦"),
    ("shrug", "🤷"),
    ("man-shrugging", "🤷‍♂️"),
    ("woman-shrugging", "🤷‍♀️"),
    ("sob", "😭"),
    ("cry", "😢"),
    ("disappointed", "😞"),
    ("pensive", "😔"),
    ("confused", "😕"),
    ("worried", "😟"),
    ("scream", "😱"),
    ("open_mouth", "😮"),
    ("astonished", "😲"),
    ("flushed", "😳"),
    ("grimacing", "😬"),
    ("neutral_face", "😐"),
    ("expressionless", "😑"),
    ("unamused", "😒"),
    ("roll_eyes", "🙄"),
    ("face_with_rolling_eyes", "🙄"),
    ("smirk", "😏"),
    ("sunglasses", "😎"),
    ("nerd_face", "🤓"),
    ("hugging_face", "🤗"),
    ("hugs", "🤗"),
    ("star-struck", "🤩"),
    ("heart_eyes", "😍"),
    ("kissing_heart", "😘"),
    ("yum", "😋"),
    ("stuck_out_tongue", "😛"),
    ("upside_down_face", "🙃"),
    ("relieved", "😌"),
    ("sleeping", "😴"),
    ("sleepy", "😪"),
    ("zipper_mouth_face", "🤐"),
    ("mask", "😷"),
    ("face_with_thermometer", "🤒"),
    ("nauseated_face", "🤢"),
    ("exploding_head", "🤯"),
    ("partying_face", "🥳"),
    ("cowboy_hat_face", "🤠"),
    ("money_mouth_face", "🤑"),
    ("innocent", "😇"),
    ("smiling_imp", "😈"),
    ("skull", "💀"),
    ("ghost", "👻"),
    ("alien", "👽"),
    ("robot_face", "🤖"),
    ("poop", "💩"),
    ("hankey", "💩"),
    ("see_no_evil", "🙈"),
    ("hear_no_evil", "🙉"),
    ("speak_no_evil", "🙊"),
    ("heavy_plus_sign", "➕"),
    ("heavy_minus_sign", "➖"),
    ("warning", "⚠️"),
    ("no_entry", "⛔"),
    ("no_entry_sign", "🚫"),
    ("question", "❓"),
    ("grey_question", "❔"),
    ("exclamation", "❗"),
    ("bangbang", "‼️"),
    ("heavy_exclamation_mark", "❗"),
    ("bulb", "💡"),
    ("memo", "📝"),
    ("pencil", "📝"),
    ("pencil2", "✏️"),
    ("calendar", "📆"),
    ("date", "📅"),
    ("clock1", "🕐"),
    ("hourglass", "⌛"),
    ("hourglass_flowing_sand", "⏳"),
    ("alarm_clock", "⏰"),
    ("stopwatch", "⏱️"),
    ("pushpin", "📌"),
    ("round_pushpin", "📍"),
    ("paperclip", "📎"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("unlock", "🔓"),
    ("key", "🔑"),
    ("hammer", "🔨"),
    ("wrench", "🔧"),
    ("gear", "⚙️"),
    ("hammer_and_wrench", "🛠️"),
    ("mag", "🔍"),
    ("bell", "🔔"),
    ("no_bell", "🔕"),
    ("loudspeaker", "📢"),
    ("mega", "📣"),
    ("speech_balloon", "💬"),
    ("thought_balloon", "💭"),
    ("email", "📧"),
    ("envelope", "✉️"),
    ("inbox_tray", "📥"),
    ("outbox_tray", "📤"),
    ("package", "📦"),
    ("chart_with_upwards_trend", "📈"),
    ("chart_with_downwards_trend", "📉"),
    ("bar_chart", "📊"),
    ("clipboard", "📋"),
    ("file_folder", "📁"),
    ("open_file_folder", "📂"),
    ("page_facing_up", "📄"),
    ("books", "📚"),
    ("book", "📖"),
    ("bookmark", "🔖"),
    ("label", "🏷️"),
    ("moneybag", "💰"),
    ("dollar", "💵"),
    ("credit_card", "💳"),
    ("gem", "💎"),
    ("trophy", "🏆"),
    ("medal", "🏅"),
    ("first_place_medal", "🥇"),
    ("dart", "🎯"),
    ("game_die", "🎲"),
    ("art", "🎨"),
    ("musical_note", "🎵"),
    ("headphones", "🎧"),
    ("microphone", "🎤"),
    ("movie_camera", "🎥"),
    ("camera", "📷"),
    ("computer", "💻"),
    ("desktop_computer", "🖥️"),
    ("keyboard", "⌨️"),
    ("iphone", "📱"),
    ("phone", "☎️"),
    ("telephone_receiver", "📞"),
    ("battery", "🔋"),
    ("electric_plug", "🔌"),
    ("satellite", "📡"),
    ("globe_with_meridians", "🌐"),
    ("earth_americas", "🌎"),
    ("sunny", "☀️"),
    ("cloud", "☁️"),
    ("umbrella", "☔"),
    ("snowflake", "❄️"),
    ("rainbow", "🌈"),
    ("ocean", "🌊"),
    ("palm_tree", "🌴"),
    ("evergreen_tree", "🌲"),
    ("seedling", "🌱"),
    ("herb", "🌿"),
    ("four_leaf_clover", "🍀"),
    ("rose", "🌹"),
    ("sunflower", "🌻"),
    ("cherry_blossom", "🌸"),
    ("coffee", "☕"),
    ("tea", "🍵"),
    ("beer", "🍺"),
    ("beers", "🍻"),
    ("wine_glass", "🍷"),
    ("champagne", "🍾"),
    ("pizza", "🍕"),
    ("hamburger", "🍔"),
    ("taco", "🌮"),
    ("doughnut", "🍩"),
    ("cake", "🍰"),
    ("birthday", "🎂"),
    ("cookie", "🍪"),
    ("apple", "🍎"),
    ("avocado", "🥑"),
    ("popcorn", "🍿"),
    ("car", "🚗"),
    ("airplane", "✈️"),
    ("train", "🚆"),
    ("bike", "🚲"),
    ("ship", "🚢"),
    ("house", "🏠"),
    ("office", "🏢"),
    ("hospital", "🏥"),
    ("dog", "🐶"),
    ("cat", "🐱"),
    ("unicorn_face", "🦄"),
    ("monkey", "🐒"),
    ("bee", "🐝"),
    ("bug", "🐛"),
    ("snail", "🐌"),
    ("turtle", "🐢"),
    ("snake", "🐍"),
    ("octopus", "🐙"),
    ("whale", "🐳"),
    ("fish", "🐟"),
    ("chicken", "🐔"),
    ("penguin", "🐧"),
    ("owl", "🦉"),
    ("panda_face", "🐼"),
    ("koala", "🐨"),
    ("tiger", "🐯"),
    ("lion_face", "🦁"),
    ("frog", "🐸"),
    ("crab", "🦀"),
    ("sloth", "🦥"),
    ("party_popper", "🎉"),
    ("confetti_ball", "🎊"),
    ("balloon", "🎈"),
    ("gift", "🎁"),
    ("ribbon", "🎀"),
    ("crown", "👑"),
    ("ring", "💍"),
    ("lipstick", "💄"),
    ("eyeglasses", "👓"),
    ("tophat", "🎩"),
    ("mortar_board", "🎓"),
    ("briefcase", "💼"),
    ("handbag", "👜"),
    ("runner", "🏃"),
    ("running", "🏃"),
    ("walking", "🚶"),
    ("dancer", "💃"),
    ("man_dancing", "🕺"),
    ("brain", "🧠"),
    ("eye", "👁️"),
    ("ear", "👂"),
    ("nose", "👃"),
    ("tongue", "👅"),
    ("lips", "👄"),
    ("baby", "👶"),
    ("boy", "👦"),
    ("girl", "👧"),
    ("man", "👨"),
    ("woman", "👩"),
    ("older_man", "👴"),
    ("older_woman", "👵"),
    ("cop", "👮"),
    ("detective", "🕵️"),
    ("construction_worker", "👷"),
    ("santa", "🎅"),
    ("angel", "👼"),
    ("ninja", "🥷"),
    ("superhero", "🦸"),
    ("zombie", "🧟"),
    ("vampire", "🧛"),
    ("mage", "🧙"),
    ("red_circle", "🔴"),
    ("large_blue_circle", "🔵"),
    ("large_green_circle", "🟢"),
    ("large_yellow_circle", "🟡"),
    ("large_orange_circle", "🟠"),
    ("white_circle", "⚪"),
    ("black_circle", "⚫"),
    ("green_heart", "💚"),
    ("blue_heart", "💙"),
    ("purple_heart", "💜"),
    ("yellow_heart", "💛"),
    ("orange_heart", "🧡"),
    ("black_heart", "🖤"),
    ("white_heart", "🤍"),
    ("broken_heart", "💔"),
    ("sparkling_heart", "💖"),
    ("two_hearts", "💕"),
    ("arrow_up", "⬆️"),
    ("arrow_down", "⬇️"),
    ("arrow_left", "⬅️"),
    ("arrow_right", "➡️"),
    ("arrows_counterclockwise", "🔄"),
    ("repeat", "🔁"),
    ("leftwards_arrow_with_hook", "↩️"),
    ("arrow_heading_up", "⤴️"),
    ("new", "🆕"),
    ("free", "🆓"),
    ("ok", "🆗"),
    ("cool", "🆒"),
    ("sos", "🆘"),
    ("information_source", "ℹ️"),
    ("recycle", "♻️"),
    ("copyright", "©️"),
    ("registered", "®️"),
    ("tm", "™️"),
    ("hash", "#️⃣"),
    ("one", "1️⃣"),
    ("two", "2️⃣"),
    ("three", "3️⃣"),
    ("four", "4️⃣"),
    ("five", "5️⃣"),
    ("ballot_box_with_check", "☑️"),
    ("radio_button", "🔘"),
    ("checkered_flag", "🏁"),
    ("triangular_flag_on_post", "🚩"),
    ("flag-us", "🇺🇸"),
    ("us", "🇺🇸"),
    ("flag-in", "🇮🇳"),
    ("flag-gb", "🇬🇧"),
    ("flag-de", "🇩🇪"),
    ("flag-fr", "🇫🇷"),
    ("flag-jp", "🇯🇵"),
    ("flag-ca", "🇨🇦"),
    ("rotating_light", "🚨"),
    ("construction", "🚧"),
    ("traffic_light", "🚥"),
    ("stop_sign", "🛑"),
    ("ship_it", "🚢"),
    ("shipit", "🚢"),
    ("dash", "💨"),
    ("sweat_drops", "💦"),
    ("droplet", "💧"),
    ("zzz", "💤"),
    ("anger", "💢"),
    ("dizzy", "💫"),
    ("speech_left", "🗨️"),
    ("salute", "🫡"),
    ("melting_face", "🫠"),
    ("saluting_face", "🫡"),
    ("face_holding_back_tears", "🥹"),
    ("pleading_face", "🥺"),
    ("smiling_face_with_tear", "🥲"),
    ("yawning_face", "🥱"),
    ("face_with_monocle", "🧐"),
    ("shushing_face", "🤫"),
    ("face_with_hand_over_mouth", "🤭"),
    ("lying_face", "🤥"),
    ("drooling_face", "🤤"),
    ("woozy_face", "🥴"),
    ("hot_face", "🥵"),
    ("cold_face", "🥶"),
    ("disguised_face", "🥸"),
    ("pinching_hand", "🤏"),
    ("writing_hand", "✍️"),
    ("eyes_closed", "😌"),
];

pub fn emoji(name: &str) -> Option<&'static str> {
    let name = name.trim_matches(':');
    // Skin tones: the base emoji.
    let base = name.split("::").next().unwrap_or(name);
    EMOJI.iter().find(|(n, _)| *n == base).map(|(_, e)| *e)
}

/// A reaction's face: the emoji, or its name.
pub fn face(name: &str) -> String {
    emoji(name).map(str::to_owned).unwrap_or_else(|| format!(":{}:", name.trim_matches(':')))
}

/// `:name:` as its emoji (anything else as it is).
pub fn face_of(token: &str) -> String {
    match token.strip_prefix(':').and_then(|t| t.strip_suffix(':')) {
        Some(n) => emoji(n).map(str::to_owned).unwrap_or_else(|| token.to_owned()),
        None => token.to_owned(),
    }
}

/// A status ("`:palm_tree:` away") with its emoji.
pub fn status(s: &str) -> String {
    match s.split_once(' ') {
        Some((e, rest)) => format!("{} {rest}", face_of(e)),
        None => face_of(s),
    }
}

/// What a message's text can point at.
#[derive(Clone, Default)]
pub struct Known {
    /// Your name (mentions of you stand out).
    pub me: String,
    pub people: Vec<String>,
    /// Channel names (without #) and their ids.
    pub channels: Vec<(String, String)>,
}

enum Part {
    Text(String),
    Code(String),
    Link(String),
    Emoji(String),
    Mention(String, bool),
    Channel(String, String),
    Bold(Vec<Part>),
    Italic(Vec<Part>),
    Strike(Vec<Part>),
}

fn boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || "([{\"'.,;:!?-/".contains(c))
}

/// The longest of `names` that `rest` starts with (case-insensitively),
/// ending at a word boundary.
fn longest<'a>(rest: &str, names: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    names
        .filter(|n| !n.is_empty() && rest.len() >= n.len() && rest.is_char_boundary(n.len()) && rest[..n.len()].eq_ignore_ascii_case(n))
        .filter(|n| boundary(rest[n.len()..].chars().next()))
        .max_by_key(|n| n.len())
}

fn inline(s: &str, k: &Known) -> Vec<Part> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    let flush = |text: &mut String, out: &mut Vec<Part>| {
        if !text.is_empty() {
            out.push(Part::Text(std::mem::take(text)));
        }
    };
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap();
        let prev = s[..i].chars().next_back();
        // `code`
        if c == '`'
            && let Some(end) = rest[1..].find('`').filter(|e| *e > 0)
        {
            flush(&mut text, &mut out);
            out.push(Part::Code(rest[1..1 + end].to_owned()));
            i += end + 2;
            continue;
        }
        // Links.
        if (rest.starts_with("https://") || rest.starts_with("http://")) && boundary(prev) {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', ')', ';', '!', '?', '>']);
            flush(&mut text, &mut out);
            out.push(Part::Link(url.to_owned()));
            i += url.len();
            continue;
        }
        // :emoji:
        if c == ':'
            && let Some(end) = rest[1..].find(':')
            && end > 0
            && rest[1..1 + end].chars().all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
            && let Some(e) = emoji(&rest[1..1 + end])
        {
            flush(&mut text, &mut out);
            out.push(Part::Emoji(e.to_owned()));
            i += end + 2;
            continue;
        }
        // @mentions and #channels.
        if c == '@' && boundary(prev) {
            let specials = ["here".to_owned(), "channel".to_owned(), "everyone".to_owned()];
            let me = [k.me.clone()];
            if let Some(n) = longest(&rest[1..], k.people.iter().chain(&specials).chain(&me)) {
                flush(&mut text, &mut out);
                let mine = n.eq_ignore_ascii_case(&k.me) || specials.contains(n);
                out.push(Part::Mention(rest[1..1 + n.len()].to_owned(), mine));
                i += 1 + n.len();
                continue;
            }
        }
        if c == '#' && boundary(prev) {
            let names: Vec<String> = k.channels.iter().map(|(n, _)| n.clone()).collect();
            if let Some(n) = longest(&rest[1..], names.iter()) {
                let id = k.channels.iter().find(|(x, _)| x == n).map(|(_, id)| id.clone()).unwrap_or_default();
                flush(&mut text, &mut out);
                out.push(Part::Channel(n.clone(), id));
                i += 1 + n.len();
                continue;
            }
        }
        // *bold*, _italic_, ~strike~
        if matches!(c, '*' | '_' | '~')
            && boundary(prev)
            && let Some(end) = rest[1..].find(c)
            && end > 0
            && !rest[1..1 + end].contains('\n')
            && !rest[1..].starts_with(' ')
            && !rest[..1 + end].ends_with(' ')
            && boundary(rest[2 + end..].chars().next())
        {
            flush(&mut text, &mut out);
            let inner = inline(&rest[1..1 + end], k);
            out.push(match c {
                '*' => Part::Bold(inner),
                '_' => Part::Italic(inner),
                _ => Part::Strike(inner),
            });
            i += end + 2;
            continue;
        }
        text.push(c);
        i += c.len_utf8();
    }
    flush(&mut text, &mut out);
    out
}

fn view_of(parts: Vec<Part>, open: Callback<String>) -> AnyView {
    parts
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => t.into_any(),
            Part::Code(t) => view! { <code class="rounded bg-zinc-100 px-1 py-0.5 font-mono text-[12px] text-rose-700 dark:bg-zinc-800 dark:text-rose-300">{t}</code> }.into_any(),
            Part::Link(u) => {
                let href = u.clone();
                view! { <a href=href target="_blank" rel="noopener" class="text-sky-700 hover:underline dark:text-sky-400">{u}</a> }.into_any()
            }
            Part::Emoji(e) => view! { <span class="text-base leading-none">{e}</span> }.into_any(),
            Part::Mention(n, mine) => {
                let cls = if mine {
                    "rounded bg-amber-200 px-0.5 font-medium text-amber-950 dark:bg-amber-700/60 dark:text-amber-50"
                } else {
                    "rounded bg-sky-100 px-0.5 text-sky-800 dark:bg-sky-900/50 dark:text-sky-200"
                };
                view! { <span class=cls>{format!("@{n}")}</span> }.into_any()
            }
            Part::Channel(n, id) => view! {
                <button class="rounded bg-sky-100 px-0.5 text-sky-800 hover:underline dark:bg-sky-900/50 dark:text-sky-200" on:click=move |_| open.run(id.clone())>{format!("#{n}")}</button>
            }
            .into_any(),
            Part::Bold(p) => view! { <b>{view_of(p, open)}</b> }.into_any(),
            Part::Italic(p) => view! { <i>{view_of(p, open)}</i> }.into_any(),
            Part::Strike(p) => view! { <s>{view_of(p, open)}</s> }.into_any(),
        })
        .collect_view()
        .into_any()
}

/// The text, rendered; `open` opens a channel by id.
pub fn render(text: &str, k: &Known, open: Callback<String>) -> AnyView {
    let mut blocks = Vec::new();
    for (n, chunk) in text.split("```").enumerate() {
        if n % 2 == 1 {
            let code = chunk.trim_matches('\n').to_owned();
            blocks.push(
                view! { <pre class="my-1 overflow-x-auto rounded-md border border-zinc-200 bg-zinc-50 p-2 font-mono text-[12px] dark:border-zinc-700 dark:bg-zinc-900">{code}</pre> }
                    .into_any(),
            );
            continue;
        }
        // Quotes (lines starting "> ") apart from the rest.
        let mut quote: Vec<&str> = Vec::new();
        let mut plain: Vec<&str> = Vec::new();
        let mut segs: Vec<(bool, String)> = Vec::new();
        for line in chunk.split('\n') {
            if let Some(q) = line.strip_prefix("> ").or(line.strip_prefix(">").filter(|_| line == ">")) {
                if !plain.is_empty() {
                    segs.push((false, plain.join("\n")));
                    plain.clear();
                }
                quote.push(q);
            } else {
                if !quote.is_empty() {
                    segs.push((true, quote.join("\n")));
                    quote.clear();
                }
                plain.push(line);
            }
        }
        if !quote.is_empty() {
            segs.push((true, quote.join("\n")));
        }
        if !plain.is_empty() {
            segs.push((false, plain.join("\n")));
        }
        for (q, s) in segs {
            let s = if n > 0 || q { s.trim_matches('\n').to_owned() } else { s };
            if s.is_empty() {
                continue;
            }
            let inner = view_of(inline(&s, k), open);
            blocks.push(if q {
                view! { <blockquote class="my-0.5 whitespace-pre-wrap border-l-4 border-zinc-300 pl-2 text-zinc-600 dark:border-zinc-600 dark:text-zinc-400">{inner}</blockquote> }.into_any()
            } else {
                view! { <div class="whitespace-pre-wrap break-words">{inner}</div> }.into_any()
            });
        }
    }
    blocks.collect_view().into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(parts: &[Part]) -> String {
        parts
            .iter()
            .map(|p| match p {
                Part::Text(t) => t.clone(),
                Part::Code(t) => format!("[code {t}]"),
                Part::Link(u) => format!("[link {u}]"),
                Part::Emoji(e) => e.to_string(),
                Part::Mention(n, m) => format!("[@{n}{}]", if *m { " me" } else { "" }),
                Part::Channel(n, id) => format!("[#{n} {id}]"),
                Part::Bold(p) => format!("[b {}]", shape(p)),
                Part::Italic(p) => format!("[i {}]", shape(p)),
                Part::Strike(p) => format!("[s {}]", shape(p)),
            })
            .collect()
    }

    #[test]
    fn slack_markup() {
        let k =
            Known { me: "Me Myself".into(), people: vec!["Ana Lima".into(), "Ana".into()], channels: vec![("eng".into(), "C1".into())] };
        let s = |t: &str| shape(&inline(t, &k));
        assert_eq!(s("hi @Ana Lima and @Me Myself, see #eng"), "hi [@Ana Lima] and [@Me Myself me], see [#eng C1]");
        assert_eq!(s("*bold _both_* and ~gone~ :tada: `x*y*`"), "[b bold [i both]] and [s gone] 🎉 [code x*y*]");
        assert_eq!(s("a*b*c 2*3 *not bold *"), "a*b*c 2*3 *not bold *", "only at word edges");
        assert_eq!(s("see https://x.io/a_b_c. ok"), "see [link https://x.io/a_b_c]. ok");
        assert_eq!(s(":custom_one: @here"), ":custom_one: [@here me]");
        assert_eq!(face("+1"), "👍");
        assert_eq!(face("partyparrot"), ":partyparrot:");
    }
}
