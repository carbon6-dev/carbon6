//! The site's pages, as Leptos components.

use leptos::prelude::*;

use crate::docs::{self, Doc};
use crate::markdown::{self, Heading, Rendered};
use crate::{Page, Site, read, render};

pub fn all(site: &Site) -> Result<Vec<Page>, String> {
    let mut pages = vec![
        Page { path: "/".into(), html: render(site, Home) },
        Page { path: "/install/".into(), html: render(site, Install) },
        Page { path: "/docs/".into(), html: render(site, DocsIndex) },
        Page { path: "/404.html".into(), html: render(site, NotFound) },
    ];

    let readme = read(&site.root.join("README.md"))?;
    let guide = guide_section(&readme).ok_or("README.md has no \"## Run\" section for the guide")?;
    let guide = markdown::render(site, "README.md", &guide);
    pages.push(Page { path: "/guide/".into(), html: render(site, move || view! { <Guide guide/> }) });

    let changelog = markdown::render(site, "CHANGELOG.md", &read(&site.root.join("CHANGELOG.md"))?);
    pages.push(Page { path: "/changelog/".into(), html: render(site, move || view! { <Changelog changelog/> }) });

    for doc in docs::all() {
        let rendered = markdown::render(site, doc.file, &read(&site.root.join(doc.file))?);
        pages.push(Page { path: format!("/docs/{}/", doc.slug), html: render(site, move || view! { <DocPage doc rendered/> }) });
    }
    Ok(pages)
}

/// The README's user guide: from "## Run" up to the next level-2 heading
/// after it that isn't about running carbon6.
fn guide_section(readme: &str) -> Option<String> {
    let start = readme.find("\n## Run\n")? + 1;
    let rest = &readme[start..];
    let end = rest.find("\n## Local development").or_else(|| rest.find("\n## License")).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

// ---------------------------------------------------------------- layout

#[component]
fn Shell(
    /// The page's own title; the site name is added.
    #[prop(into)]
    title: String,
    #[prop(into)] description: String,
    /// The page's path, for the canonical link and the current nav item.
    #[prop(into)]
    path: String,
    #[prop(optional)] wide: bool,
    children: Children,
) -> impl IntoView {
    let site = expect_context::<Site>();
    let full_title = if title.is_empty() {
        "carbon6 — your attention on what matters, agents on everything else".to_string()
    } else {
        format!("{title} · carbon6")
    };
    let canonical = format!("{}{}", site.base_url, if path == "/404.html" { "/" } else { &path });
    let current = |prefix: &'static str| path.starts_with(prefix).then_some("page");
    let nav = [("/docs/", "Docs"), ("/guide/", "Guide"), ("/install/", "Install"), ("/changelog/", "Changelog")];
    view! {
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <title>{full_title.clone()}</title>
                <meta name="description" content=description.clone()/>
                <meta name="color-scheme" content="light dark"/>
                <link rel="canonical" href=canonical.clone()/>
                <link rel="icon" href="/icon.svg" type="image/svg+xml"/>
                <link rel="apple-touch-icon" href="/icon.png"/>
                <link rel="stylesheet" href=format!("/style.css?v={}", site.asset_version)/>
                {og("og:type", "website")}
                {og("og:site_name", "carbon6")}
                {og("og:title", full_title)}
                {og("og:description", description)}
                {og("og:url", canonical)}
                {og("og:image", format!("{}/icon.png", site.base_url))}
            </head>
            <body>
                <a class="skip" href="#main">"Skip to content"</a>
                <header class="top">
                    <div class="top-inner">
                        <a class="brand" href="/">
                            <img src="/icon.svg" alt="" width="28" height="28"/>
                            <span>"carbon6"</span>
                        </a>
                        <nav aria-label="Site">
                            {nav
                                .into_iter()
                                .map(|(href, label)| view! { <a href=href aria-current=current(href)>{label}</a> })
                                .collect_view()}
                            <a class="gh" href=site.repo_url.clone() aria-label="carbon6 on GitHub">
                                <GitHubIcon/>
                                <span class="gh-label">"GitHub"</span>
                            </a>
                        </nav>
                    </div>
                </header>
                <main id="main" class:wide=wide>
                    {children()}
                </main>
                <footer class="bottom">
                    <div class="bottom-inner">
                        <span>
                            "carbon6 " {site.version.clone()} " · "
                            <a href=format!("{}/blob/main/LICENSE-MIT", site.repo_url)>"MIT"</a> " or "
                            <a href=format!("{}/blob/main/LICENSE-APACHE", site.repo_url)>"Apache-2.0"</a>
                        </span>
                        <span>
                            <a href=site.repo_url.clone()>"Source"</a> " · "
                            <a href=format!("{}/releases", site.repo_url)>"Releases"</a> " · "
                            <a href=format!("{}/security", site.repo_url)>"Report a vulnerability"</a>
                        </span>
                    </div>
                </footer>
            </body>
        </html>
    }
}

/// An Open Graph `<meta property=… content=…>` (the view macro reserves
/// `property`).
fn og(property: &'static str, content: impl Into<String>) -> impl IntoView {
    leptos::html::meta().attr("property", property).attr("content", content.into())
}

#[component]
fn GitHubIcon() -> impl IntoView {
    view! {
        <span class="icon" inner_html=r#"<svg viewBox="0 0 16 16" width="18" height="18" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>"#></span>
    }
}

/// A shell command block; each line is one command (or a `#` comment).
#[component]
fn Code(#[prop(into)] lines: Vec<String>, #[prop(optional, into)] label: Option<String>) -> impl IntoView {
    view! {
        <figure class="code">
            {label.map(|l| view! { <figcaption>{l}</figcaption> })}
            <pre><code>{lines.join("\n")}</code></pre>
        </figure>
    }
}

// ---------------------------------------------------------------- home

const ARCHITECTURE: &str = r##"<svg viewBox="0 0 720 300" role="img" aria-labelledby="arch-title arch-desc">
<title id="arch-title">How carbon6 fits together</title>
<desc id="arch-desc">A browser or the c6 command line reaches the head, c6h. Workers (c6w) on a laptop, a dev box and a build server connect out to the head, which also keeps the knowledge graph.</desc>
<defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0 L10 5 L0 10 z" class="d-arrow"/></marker></defs>
<g class="d-lines">
<path d="M190 100 L272 124" marker-end="url(#arrow)"/>
<path d="M190 200 L272 176" marker-end="url(#arrow)"/>
<path d="M530 48 L448 104" marker-end="url(#arrow)"/>
<path d="M530 114 L448 134" marker-end="url(#arrow)"/>
<path d="M530 180 L448 164" marker-end="url(#arrow)"/>
</g>
<g class="d-box"><rect x="20" y="70" width="170" height="60" rx="10"/><text x="38" y="96" class="d-title d-plain">browser, phone</text><text x="38" y="116" class="d-sub">the web UI, passkeys</text></g>
<g class="d-box"><rect x="20" y="170" width="170" height="60" rx="10"/><text x="38" y="196" class="d-title">c6</text><text x="38" y="216" class="d-sub">CLI, scripts, API tokens</text></g>
<g class="d-head"><rect x="275" y="40" width="170" height="230" rx="14"/><text x="295" y="84" class="d-title d-big">c6h</text><text x="295" y="106" class="d-sub">the head</text><line x1="295" y1="122" x2="425" y2="122"/>
<text x="295" y="150" class="d-sub">web UI and API</text><text x="295" y="174" class="d-sub">settings, secrets</text><text x="295" y="198" class="d-sub">every session's log</text><text x="295" y="222" class="d-sub">automations, approvals</text><text x="295" y="246" class="d-sub">the knowledge graph</text></g>
<g class="d-box"><rect x="530" y="24" width="170" height="48" rx="10"/><text x="546" y="45" class="d-title">c6w</text><text x="546" y="62" class="d-sub">your laptop · macOS</text></g>
<g class="d-box"><rect x="530" y="90" width="170" height="48" rx="10"/><text x="546" y="111" class="d-title">c6w</text><text x="546" y="128" class="d-sub">a dev box · Linux</text></g>
<g class="d-box"><rect x="530" y="156" width="170" height="48" rx="10"/><text x="546" y="177" class="d-title">c6w</text><text x="546" y="194" class="d-sub">a build server</text></g>
</svg>"##;

/// A reason carbon6 multiplies what you get done: what it removes, and the
/// features that remove it.
struct Lever {
    title: &'static str,
    text: &'static str,
    how: &'static [&'static str],
}

const LEVERS: &[Lever] = &[
    Lever {
        title: "Every machine, without friction",
        text: "Your laptop, dev boxes and build servers are one click away, from a browser or your phone. No SSH sessions to juggle, no terminal tabs to babysit.",
        how: &[
            "Start a session on any machine and watch it work live",
            "Run the same task across a fleet of machines at once",
            "Sessions keep going when the network doesn't",
        ],
    },
    Lever {
        title: "Knowledge that captures itself",
        text: "Every session leaves behind what it learned. The memex turns sessions, documents and mail into a graph of people, projects, hosts and topics, and every new session starts from it. You never explain the same thing twice.",
        how: &[
            "Each session's digest lands in the knowledge graph on its own",
            "New sessions start with a brief of what's already known",
            "Agents search everything by keyword and by meaning",
        ],
    },
    Lever {
        title: "All your context, one platform",
        text: "Mail, calendar, contacts, documents and papers live beside your agents, not scattered across a dozen single-purpose tools. An agent drafting a reply knows the thread, the project and the person.",
        how: &[
            "Gmail, Calendar and Contacts, synced both ways",
            "Documents and papers, with the citations between them",
            "Apps for your own workflows, built from configuration, not code",
        ],
    },
    Lever {
        title: "Agents that don't wait on you",
        text: "Agents keep working and queue what needs you. You look when you choose to, decide in a tap, and get back to the work only you can do.",
        how: &[
            "Questions become action items, not interruptions",
            "Risky steps wait for your approval, from your phone",
            "Schedules, webhooks and new mail start work without you",
        ],
    },
];

struct Feature {
    title: &'static str,
    text: &'static str,
}

const FEATURES: &[Feature] = &[
    Feature {
        title: "The model you want",
        text: "Anthropic, OpenAI with a key or your ChatGPT plan, any OpenAI-compatible server such as Ollama or vLLM, or each worker's own signed-in Claude Code.",
    },
    Feature {
        title: "Tools that fit the model",
        text: "Claude Code-style tools for Claude, Codex-style for GPT, a generic set for the rest. MCP servers, skills, subagents, plans and web search with any model.",
    },
    Feature {
        title: "An IDE in the browser",
        text: "Browse a worker's files, review its git changes and open terminals on it, from any browser, phone included.",
    },
    Feature {
        title: "An OS for agents",
        text: "Realms keep apps apart. Apps are built from collections, decisions, triggers, series and views, by configuration alone, and installed as packages.",
    },
    Feature {
        title: "Usage and budgets",
        text: "What every session, model and project costs, with budgets that stop the spending where you set them.",
    },
    Feature {
        title: "Observable",
        text: "The head exports OpenTelemetry GenAI traces over OTLP, so every model call and tool call can land in the tracing you already run.",
    },
];

#[component]
fn Home() -> impl IntoView {
    let site = expect_context::<Site>();
    let v = site.version.clone();
    view! {
        <Shell
            title=""
            description="carbon6 is a self-hosted platform for AI agents that takes the friction out of your work: every machine, your mail and calendar, and a knowledge graph that builds itself, in one place. Your attention goes to what matters; agents do the rest."
            path="/"
            wide=true
        >
            <section class="hero">
                <div class="hero-text">
                    <p class="eyebrow">
                        <a href="/changelog/">"carbon6 " {v.clone()}</a>
                        " · open source · self-hosted"
                    </p>
                    <h1>"Your attention on what matters. Agents on everything else."</h1>
                    <p class="lede">
                        "Attention is the one resource you can't add more of. carbon6 clears away the friction around it: every machine you work on, your mail and calendar, and everything your agents learn, in one place. You choose what matters; agents carry the rest. That's how one person does the work of ten."
                    </p>
                    <div class="actions">
                        <a class="button primary" href="/install/">"Install"</a>
                        <a class="button" href="/docs/">"Read the docs"</a>
                        <a class="button ghost" href=site.repo_url.clone()>
                            <GitHubIcon/>
                            "GitHub"
                        </a>
                    </div>
                </div>
            </section>

            <section class="band">
                <h2>"Where the 10x comes from"</h2>
                <p class="section-lede">"Agents are fast. What slows you down is everything around them: reaching the right machine, re-explaining context, switching between tools, answering every question as it comes up. carbon6 removes each of those."</p>
                <div class="levers">
                    {LEVERS
                        .iter()
                        .enumerate()
                        .map(|(i, l)| view! {
                            <div class="lever">
                                <span class="lever-n">{format!("0{}", i + 1)}</span>
                                <h3>{l.title}</h3>
                                <p>{l.text}</p>
                                <ul class="checks">{l.how.iter().map(|h| view! { <li>{*h}</li> }).collect_view()}</ul>
                            </div>
                        })
                        .collect_view()}
                </div>
            </section>

            <section class="band">
                <h2>"How it fits together"</h2>
                <figure class="diagram">
                    <div class="diagram-scroll" inner_html=ARCHITECTURE></div>
                    <figcaption>
                        "Workers and the memex dial out to the head over mutual TLS, so they listen on no port. You reach the head from a browser, a phone or the "
                        <code>"c6"</code>
                        " command line."
                    </figcaption>
                </figure>
                <div class="parts">
                    <div class="part">
                        <h3><code>"c6h"</code>" the head"</h3>
                        <p>"Runs in one place. It owns all configuration, credentials and session data, keeps the knowledge graph (sessions, documents, mail and photos), and serves the web UI."</p>
                    </div>
                    <div class="part">
                        <h3><code>"c6w"</code>" a worker"</h3>
                        <p>"Runs headless on each dev machine, macOS or Linux, and runs agent sessions there."</p>
                    </div>
                    <div class="part">
                        <h3><code>"c6"</code>" the CLI"</h3>
                        <p>"Runs sessions headless, for scripts and terminals, with an API token."</p>
                    </div>
                </div>
            </section>

            <section class="band">
                <h2>"Under the hood"</h2>
                <div class="features">
                    {FEATURES
                        .iter()
                        .map(|f| view! {
                            <div class="feature">
                                <h3>{f.title}</h3>
                                <p>{f.text}</p>
                            </div>
                        })
                        .collect_view()}
                </div>
            </section>

            <section class="band split">
                <div>
                    <h2>"Private by default"</h2>
                    <p class="muted">"Your mail, your code and your machines in one place have to be safe there. Agents run shell commands as you, so the head is as sensitive as a shell on every worker. carbon6 is built for that."</p>
                    <ul class="checks">
                        <li>"Self-hosted: your data stays on machines you own."</li>
                        <li>"The head binds only loopback, private and tailnet addresses unless you say otherwise, and answers only to its own names."</li>
                        <li>"Sign-in with passkeys and one-time login links made on the head; realm-scoped API tokens for scripts."</li>
                        <li>"Workers enroll with a one-time token that pins the head's CA, then connect with mutual TLS."</li>
                        <li>"API keys, OAuth tokens and your secrets are encrypted in the head's database."</li>
                    </ul>
                </div>
                <div>
                    <h2>"Try it"</h2>
                    <Code
                        label="macOS"
                        lines=vec![
                            "brew tap carbon6-dev/carbon6".to_string(),
                            "brew install c6h c6w".into(),
                            "brew services start c6h".into(),
                            "c6h --config $(brew --prefix)/etc/c6h/c6h.toml login-link".into(),
                        ]
                    />
                    <Code
                        label="Ubuntu 22.04 or later"
                        lines=vec![
                            format!("curl -LO {}/releases/download/v{v}/c6h_{v}_$(dpkg --print-architecture).deb", site.repo_url),
                            format!("sudo apt install ./c6h_{v}_*.deb"),
                            "sudo -u c6h c6h --config /etc/c6h/c6h.toml login-link".into(),
                        ]
                    />
                    <p class="muted">
                        "Open the link it prints, add a model under Settings, then add a worker. "
                        <a href="/install/">"The full install guide"</a>
                        "."
                    </p>
                </div>
            </section>
        </Shell>
    }
}

// ---------------------------------------------------------------- install

#[component]
fn Install() -> impl IntoView {
    let site = expect_context::<Site>();
    let v = site.version.clone();
    let release = format!("{}/releases/download/v{v}", site.repo_url);
    view! {
        <Shell
            title="Install"
            description="Install the carbon6 head, workers, memex and CLI on macOS with Homebrew, on Ubuntu with .deb packages, or from source."
            path="/install/"
        >
            <article class="prose">
                <h1>"Install"</h1>
                <p class="lede">
                    "You need one head, and a worker on every machine you want agents to work on. The head can share a machine with a worker. Version "
                    {v.clone()}
                    " is the latest; "
                    <a href="/changelog/">"see what changed"</a>
                    "."
                </p>

                <h2 id="macos">"macOS, with Homebrew"</h2>
                <p>"Bottles for Apple Silicon and Intel, for macOS 11 and later. They need neither Xcode nor the Command Line Tools. The services run as you, under launchd."</p>
                <Code lines=vec![
                    "brew tap carbon6-dev/carbon6".to_string(),
                    "brew install c6h c6w c6".into(),
                    "brew services start c6h".into(),
                ]/>
                <ul>
                    <li>"The head's config is " <code>"$(brew --prefix)/etc/c6h/c6h.toml"</code> " (kept across upgrades), its data " <code>"$(brew --prefix)/var/c6h"</code> "."</li>
                    <li>"Sign in with " <code>"c6h --config $(brew --prefix)/etc/c6h/c6h.toml login-link"</code> "."</li>
                    <li>"A worker: " <code>"c6w enroll <token>"</code> ", then " <code>"brew services start c6w"</code> ". It starts through a login shell, so sessions get your " <code>"PATH"</code> "."</li>
                    <li>"Logs are in " <code>"$(brew --prefix)/var/log/"</code> ". " <code>"brew upgrade"</code> " upgrades them."</li>
                </ul>

                <h2 id="ubuntu">"Ubuntu, with .deb packages"</h2>
                <p>"Built on Ubuntu 22.04 for amd64 and arm64, they install on 22.04 and every later release, with systemd units."</p>
                <Code lines=vec![
                    "arch=$(dpkg --print-architecture)".to_string(),
                    format!("curl -LO {release}/c6h_{v}_$arch.deb"),
                    format!("sudo apt install ./c6h_{v}_$arch.deb"),
                ]/>
                <ul>
                    <li>
                        <strong>"c6h"</strong>
                        " runs as the " <code>"c6h"</code> " service and starts on install. Config: " <code>"/etc/c6h/c6h.toml"</code>
                        "; state: " <code>"/var/lib/c6h"</code> ". Sign in with "
                        <code>"sudo -u c6h c6h --config /etc/c6h/c6h.toml login-link"</code> "."
                    </li>
                    <li>
                        <strong>"c6w"</strong>
                        " (" <code>{format!("c6w_{v}_$arch.deb")}</code> ") runs as you, since sessions work on your files: "
                        <code>"c6w enroll <token>"</code> ", then " <code>"sudo systemctl enable --now c6w@$USER"</code>
                        " to start at boot, or " <code>"systemctl --user enable --now c6w"</code> " for your session."
                    </li>
                    <li><strong>"c6"</strong> " (" <code>{format!("c6_{v}_$arch.deb")}</code> "), the command line, on any machine that reaches the head."</li>
                    <li>"Logs: " <code>"journalctl -u c6h"</code> ". Installing a newer package restarts its services; removing one keeps its data."</li>
                </ul>

                <h2 id="linux">"Other Linux"</h2>
                <p>
                    "The four binaries, for amd64 and arm64, glibc 2.35 or later: "
                    <a href=format!("{release}/carbon6-{v}-linux-amd64.tar.gz")>{format!("carbon6-{v}-linux-amd64.tar.gz")}</a>
                    ", "
                    <a href=format!("{release}/carbon6-{v}-linux-arm64.tar.gz")>{format!("carbon6-{v}-linux-arm64.tar.gz")}</a>
                    ". Every file of a release is listed, with checksums, on "
                    <a href=format!("{}/releases/tag/v{v}", site.repo_url)>"its release page"</a>
                    "."
                </p>

                <h2 id="source">"From source"</h2>
                <p>
                    "With Docker alone, " <code>"make docker-build"</code> " builds Linux binaries into "
                    <code>"dist/"</code> ". With a local toolchain you need Rust with the "
                    <code>"wasm32-unknown-unknown"</code> " target and " <a href="https://trunkrs.dev">"Trunk"</a> ":"
                </p>
                <Code lines=vec![
                    format!("git clone {}.git", site.repo_url),
                    "cd carbon6".into(),
                    "make build".into(),
                ]/>

                <h2 id="first-run">"First run"</h2>
                <ol>
                    <li>
                        <strong>"Sign in."</strong>
                        " Run " <code>"c6h login-link"</code> " on the head and open the link it prints (it works once, for 15 minutes). Add a passkey under "
                        <em>"Settings → Sign-in"</em> " and use it from then on."
                    </li>
                    <li>
                        <strong>"Add a model."</strong>
                        " Add an LLM connection under " <em>"Settings"</em> ": an API key, your ChatGPT plan, Claude Code on the workers, or a local server such as Ollama ("
                        <code>"http://127.0.0.1:11434/v1"</code> ", no key)."
                    </li>
                    <li>
                        <strong>"Add a worker."</strong>
                        " On the " <em>"Workers"</em> " page, click " <em>"Add worker"</em>
                        " and run the command it prints on the worker machine. Then start the worker's service, or run "
                        <code>"c6w run"</code> "."
                    </li>
                    <li>
                        <strong>"Start a session"</strong>
                        " on that worker, in a folder of your choice."
                    </li>
                </ol>
                <p>
                    "The head binds only loopback, private and tailnet addresses by default. To reach it from outside a tailnet or "
                    <code>"localhost"</code> ", turn on HTTPS with " <code>"ui_tls_head_ca = true"</code>
                    ". The " <a href="/guide/">"guide"</a> " covers this and everything after the first session."
                </p>
            </article>
        </Shell>
    }
}

// ---------------------------------------------------------------- guide, changelog

/// The table of contents for a markdown page, from its headings of `level`.
#[component]
fn Toc(headings: Vec<Heading>, level: u8) -> impl IntoView {
    let items: Vec<_> = headings.into_iter().filter(|h| h.level == level).collect();
    (!items.is_empty()).then(|| {
        view! {
            <nav class="toc" aria-label="On this page">
                <p class="toc-title">"On this page"</p>
                <ul>
                    {items.into_iter().map(|h| view! { <li><a href=format!("#{}", h.id)>{h.text}</a></li> }).collect_view()}
                </ul>
            </nav>
        }
    })
}

#[component]
fn Guide(guide: Rendered) -> impl IntoView {
    let site = expect_context::<Site>();
    view! {
        <Shell
            title="Guide"
            description="Using carbon6: sign-in, workers, the command line, the assistant, automations, the memex, and the rest of the head's features."
            path="/guide/"
            wide=true
        >
            <div class="doc-layout no-sidebar">
                <article class="prose">
                    <h1>"Guide"</h1>
                    <p class="lede">"Everything after installing: signing in, adding workers, and the head's features one by one."</p>
                    <div inner_html=guide.html></div>
                    <p class="source">
                        "This guide is the README's; "
                        <a href=format!("{}/blob/main/README.md", site.repo_url)>"edit it on GitHub"</a>
                        "."
                    </p>
                </article>
                <aside class="doc-aside">
                    <Toc headings=guide.toc level=3/>
                </aside>
            </div>
        </Shell>
    }
}

#[component]
fn Changelog(changelog: Rendered) -> impl IntoView {
    let site = expect_context::<Site>();
    view! {
        <Shell title="Changelog" description="Every carbon6 release and what changed in it." path="/changelog/">
            <article class="prose changelog">
                <h1>"Changelog"</h1>
                <p class="lede">
                    "One line per change. The packages for every release are on "
                    <a href=format!("{}/releases", site.repo_url)>"GitHub Releases"</a>
                    "."
                </p>
                <div inner_html=changelog.html></div>
            </article>
        </Shell>
    }
}

// ---------------------------------------------------------------- docs

#[component]
fn DocsIndex() -> impl IntoView {
    view! {
        <Shell
            title="Docs"
            description="The carbon6 design documents: requirements, architecture, the agent OS, packages, the assistant, subagents, the memex and more."
            path="/docs/"
        >
            <article class="prose">
                <h1>"Docs"</h1>
                <p class="lede">
                    "carbon6 is designed in the open. Each document records the goals, the decisions and, once built, how the build differs from the design. To use carbon6, start with the "
                    <a href="/install/">"install guide"</a>
                    " and the "
                    <a href="/guide/">"guide"</a>
                    "."
                </p>
            </article>
            {docs::GROUPS
                .iter()
                .map(|group| view! {
                    <section class="doc-group">
                        <h2>{group.name}</h2>
                        <div class="cards">
                            {group
                                .docs
                                .iter()
                                .map(|doc| view! {
                                    <a class="card" href=format!("/docs/{}/", doc.slug)>
                                        <h3>{doc.label}</h3>
                                        <p>{doc.summary}</p>
                                    </a>
                                })
                                .collect_view()}
                        </div>
                    </section>
                })
                .collect_view()}
        </Shell>
    }
}

#[component]
fn DocPage(doc: &'static Doc, rendered: Rendered) -> impl IntoView {
    let site = expect_context::<Site>();
    let title = rendered.title.clone().unwrap_or_else(|| doc.label.to_string());
    view! {
        <Shell title=doc.label description=doc.summary path=format!("/docs/{}/", doc.slug) wide=true>
            <div class="doc-layout">
                <nav class="doc-nav" aria-label="Docs">
                        {docs::GROUPS
                            .iter()
                            .map(|group| view! {
                                <p class="doc-nav-group">{group.name}</p>
                                <ul>
                                    {group
                                        .docs
                                        .iter()
                                        .map(|d| view! {
                                            <li>
                                                <a href=format!("/docs/{}/", d.slug) aria-current=(d.slug == doc.slug).then_some("page")>
                                                    {d.label}
                                                </a>
                                            </li>
                                        })
                                        .collect_view()}
                                </ul>
                            })
                            .collect_view()}
                </nav>
                <article class="prose">
                    <h1>{title}</h1>
                    <div inner_html=rendered.html></div>
                    <p class="source">
                        <a href=format!("{}/blob/main/{}", site.repo_url, doc.file)>{format!("{} on GitHub", doc.file)}</a>
                    </p>
                </article>
                <aside class="doc-aside">
                    <Toc headings=rendered.toc level=2/>
                </aside>
            </div>
        </Shell>
    }
}

// ---------------------------------------------------------------- 404

#[component]
fn NotFound() -> impl IntoView {
    view! {
        <Shell title="Not found" description="This page doesn't exist." path="/404.html">
            <article class="prose center">
                <h1>"Not found"</h1>
                <p class="lede">"There's no page here. It may have moved when the docs did."</p>
                <p><a class="button primary" href="/">"Home"</a> " " <a class="button" href="/docs/">"Docs"</a></p>
            </article>
        </Shell>
    }
}
