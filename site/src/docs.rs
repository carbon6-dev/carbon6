//! The design docs the site publishes, in the order and groups it shows them.
//! A doc in docs/ that isn't listed here is left off the site, and links to
//! it go to GitHub instead.

pub struct Doc {
    /// The page is /docs/<slug>/.
    pub slug: &'static str,
    /// Relative to the repository root.
    pub file: &'static str,
    /// The name in the sidebar and the docs index.
    pub label: &'static str,
    pub summary: &'static str,
}

pub struct Group {
    pub name: &'static str,
    pub docs: &'static [Doc],
}

pub const GROUPS: &[Group] = &[
    Group {
        name: "Start here",
        docs: &[
            Doc {
                slug: "requirements",
                file: "docs/requirements.md",
                label: "Requirements",
                summary: "What carbon6 is for, and what it must and must not do.",
            },
            Doc {
                slug: "architecture",
                file: "docs/architecture.md",
                label: "Architecture",
                summary: "The head, workers and memex, the protocol between them, and where every piece of state lives.",
            },
        ],
    },
    Group {
        name: "An operating system for agents",
        docs: &[
            Doc {
                slug: "os",
                file: "docs/os.md",
                label: "The OS",
                summary: "Realms, grants, collections, decisions, triggers, series and views: apps built from configuration alone.",
            },
            Doc {
                slug: "packages",
                file: "docs/packages.md",
                label: "Packages",
                summary: "Apps, kits and libraries, installed from a registry with the permissions they ask for.",
            },
            Doc {
                slug: "apps",
                file: "docs/apps.md",
                label: "Apps: tools and daemons",
                summary: "Proposed: an app's code apart from the data made with it, tools that run in your realm, and code that changes only by upgrade.",
            },
            Doc {
                slug: "explorer",
                file: "docs/explorer.md",
                label: "Realm explorer",
                summary: "Everything a realm holds, and how its parts connect, one click from each other.",
            },
        ],
    },
    Group {
        name: "Features",
        docs: &[
            Doc {
                slug: "assistant",
                file: "docs/assistant.md",
                label: "The assistant",
                summary: "An agent that can do anything you can do in the UI, and answer questions about your setup.",
            },
            Doc {
                slug: "subagents",
                file: "docs/subagents.md",
                label: "Subagents",
                summary: "Subagents as sessions of their own: watched live, stopped, and continued with their history.",
            },
            Doc {
                slug: "memex",
                file: "docs/memex.md",
                label: "Memex",
                summary: "A knowledge store for your sessions, documents, mail and photos, searchable by meaning.",
            },
            Doc {
                slug: "decks",
                file: "docs/decks.md",
                label: "Slide decks",
                summary: "From a prompt to an outline, a look, and slides checked by eye.",
            },
        ],
    },
    Group {
        name: "Project",
        docs: &[
            Doc {
                slug: "releases",
                file: "docs/releases.md",
                label: "Releases and CI",
                summary: "How a release is cut, built, tested and published, and how CI runs the same commands you do.",
            },
            Doc {
                slug: "site",
                file: "docs/site.md",
                label: "The website",
                summary: "How this site is generated with Leptos and published to GitHub Pages.",
            },
        ],
    },
    Group {
        name: "Designs on the shelf",
        docs: &[
            Doc {
                slug: "measurement",
                file: "docs/measurement.md",
                label: "Measurement",
                summary: "Measuring the leverage agents give you. Designed, not built.",
            },
            Doc {
                slug: "follow-through",
                file: "docs/follow-through.md",
                label: "Follow-through",
                summary: "Sessions that see their work through after the push. Built, then mostly removed.",
            },
        ],
    },
];

pub fn all() -> impl Iterator<Item = &'static Doc> {
    GROUPS.iter().flat_map(|g| g.docs.iter())
}

/// The site path of a repository file, if the site publishes it.
pub fn site_path(file: &str) -> Option<String> {
    match file {
        "README.md" => Some("/guide/".into()),
        "CHANGELOG.md" => Some("/changelog/".into()),
        _ => all().find(|d| d.file == file).map(|d| format!("/docs/{}/", d.slug)),
    }
}
