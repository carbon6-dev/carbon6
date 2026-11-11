//! Lucide 1.66.0 (lucide-static), vendored 24-unit path data; no runtime dependency.
//! See ../LUCIDE-LICENSE for Lucide's ISC and inherited Feather MIT notices.
//! Chrome uses drawn icons, never device emoji. User content stays untouched.

use leptos::prelude::*;

/// Approved sizes, with an absolute 1.5px stroke at each size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    S14,
    S16,
    S20,
}

impl Size {
    pub const fn pixels(self) -> u8 {
        match self {
            Self::S14 => 14,
            Self::S16 => 16,
            Self::S20 => 20,
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::S14 => "size-3.5",
            Self::S16 => "size-4",
            Self::S20 => "size-5",
        }
    }

    fn stroke(self) -> f64 {
        1.5 * 24.0 / f64::from(self.pixels())
    }
}

/// Names mirror Lucide's kebab-case asset names. One source for views and markup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Archive,
    ArchiveRestore,
    PanelsTopLeft,
    FileDiff,
    SquareTerminal,
    Bell,
    BellDot,
    BellOff,
    Brain,
    LayoutGrid,
    Info,
    CircleHelp,
    User,
    Users,
    Plus,
    Square,
    Quote,
    List,
    ListOrdered,
    Calendar,
    CalendarClock,
    LayoutTemplate,
    File,
    FileText,
    KeyRound,
    Check,
    TriangleAlert,
    Clock,
    Play,
    Circle,
    CircleDot,
    CircleX,
    CircleDashed,
    ChevronDown,
    ChevronUp,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowRightToLine,
    ArrowUpRight,
    ArrowDownRight,
    CornerDownRight,
    Undo2,
    RotateCcw,
    RefreshCw,
    Pencil,
    PenLine,
    MessageCircle,
    MessageSquare,
    MessageSquareReply,
    Bookmark,
    Link,
    Paperclip,
    Lock,
    Menu,
    Trash2,
    Mail,
    Diamond,
    Zap,
    ArrowLeftRight,
    ChartLine,
    SmilePlus,
    Folder,
    Package,
    ChevronLeft,
    ChevronRight,
    X,
    Ellipsis,
    Star,
    SquareArrowOutUpRight,
    ExternalLink,
    LocateFixed,
    Minus,
    Maximize,
    Minimize,
    Search,
    SlidersHorizontal,
    Sparkles,
    SquareCheck,
    Presentation,
    Layers,
    Box,
    Bot,
    ShieldCheck,
    LayoutPanelLeft,
    Settings,
    Globe,
    Dot,
    Film,
    Music,
    Sheet,
    FileArchive,
    Image,
    SendHorizontal,
    Server,
    ArrowDownToLine,
    ArrowDownToDot,
    PanelLeftClose,
    PanelLeftOpen,
    Pause,
    LoaderCircle,
    Target,
    Milestone,
    ChevronsLeft,
    ChevronsRight,
    Download,
    Copy,
}

impl Icon {
    pub const ALL: &[Self] = &[
        Self::Archive,
        Self::ArchiveRestore,
        Self::PanelsTopLeft,
        Self::FileDiff,
        Self::SquareTerminal,
        Self::Bell,
        Self::BellDot,
        Self::BellOff,
        Self::Brain,
        Self::LayoutGrid,
        Self::Info,
        Self::CircleHelp,
        Self::User,
        Self::Users,
        Self::Plus,
        Self::Square,
        Self::Quote,
        Self::List,
        Self::ListOrdered,
        Self::Calendar,
        Self::CalendarClock,
        Self::LayoutTemplate,
        Self::File,
        Self::FileText,
        Self::KeyRound,
        Self::Check,
        Self::TriangleAlert,
        Self::Clock,
        Self::Play,
        Self::Circle,
        Self::CircleDot,
        Self::CircleX,
        Self::CircleDashed,
        Self::ChevronDown,
        Self::ChevronUp,
        Self::ArrowUp,
        Self::ArrowDown,
        Self::ArrowLeft,
        Self::ArrowRight,
        Self::ArrowRightToLine,
        Self::ArrowUpRight,
        Self::ArrowDownRight,
        Self::CornerDownRight,
        Self::Undo2,
        Self::RotateCcw,
        Self::RefreshCw,
        Self::Pencil,
        Self::PenLine,
        Self::MessageCircle,
        Self::MessageSquare,
        Self::MessageSquareReply,
        Self::Bookmark,
        Self::Link,
        Self::Paperclip,
        Self::Lock,
        Self::Menu,
        Self::Trash2,
        Self::Mail,
        Self::Diamond,
        Self::Zap,
        Self::ArrowLeftRight,
        Self::ChartLine,
        Self::SmilePlus,
        Self::Folder,
        Self::Package,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::X,
        Self::Ellipsis,
        Self::Star,
        Self::SquareArrowOutUpRight,
        Self::ExternalLink,
        Self::LocateFixed,
        Self::Minus,
        Self::Maximize,
        Self::Minimize,
        Self::Search,
        Self::SlidersHorizontal,
        Self::Sparkles,
        Self::SquareCheck,
        Self::Presentation,
        Self::Layers,
        Self::Box,
        Self::Bot,
        Self::ShieldCheck,
        Self::LayoutPanelLeft,
        Self::Settings,
        Self::Globe,
        Self::Dot,
        Self::Film,
        Self::Music,
        Self::Sheet,
        Self::FileArchive,
        Self::Image,
        Self::SendHorizontal,
        Self::Server,
        Self::ArrowDownToLine,
        Self::ArrowDownToDot,
        Self::PanelLeftClose,
        Self::PanelLeftOpen,
        Self::Pause,
        Self::LoaderCircle,
        Self::Target,
        Self::Milestone,
        Self::ChevronsLeft,
        Self::ChevronsRight,
        Self::Download,
        Self::Copy,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::ArchiveRestore => "archive-restore",
            Self::PanelsTopLeft => "panels-top-left",
            Self::FileDiff => "file-diff",
            Self::SquareTerminal => "square-terminal",
            Self::Bell => "bell",
            Self::BellDot => "bell-dot",
            Self::BellOff => "bell-off",
            Self::Brain => "brain",
            Self::LayoutGrid => "layout-grid",
            Self::Info => "info",
            Self::CircleHelp => "circle-help",
            Self::User => "user",
            Self::Users => "users",
            Self::Plus => "plus",
            Self::Square => "square",
            Self::Quote => "quote",
            Self::List => "list",
            Self::ListOrdered => "list-ordered",
            Self::Calendar => "calendar",
            Self::CalendarClock => "calendar-clock",
            Self::LayoutTemplate => "layout-template",
            Self::File => "file",
            Self::FileText => "file-text",
            Self::KeyRound => "key-round",
            Self::Check => "check",
            Self::TriangleAlert => "triangle-alert",
            Self::Clock => "clock",
            Self::Play => "play",
            Self::Circle => "circle",
            Self::CircleDot => "circle-dot",
            Self::CircleX => "circle-x",
            Self::CircleDashed => "circle-dashed",
            Self::ChevronDown => "chevron-down",
            Self::ChevronUp => "chevron-up",
            Self::ArrowUp => "arrow-up",
            Self::ArrowDown => "arrow-down",
            Self::ArrowLeft => "arrow-left",
            Self::ArrowRight => "arrow-right",
            Self::ArrowRightToLine => "arrow-right-to-line",
            Self::ArrowUpRight => "arrow-up-right",
            Self::ArrowDownRight => "arrow-down-right",
            Self::CornerDownRight => "corner-down-right",
            Self::Undo2 => "undo-2",
            Self::RotateCcw => "rotate-ccw",
            Self::RefreshCw => "refresh-cw",
            Self::Pencil => "pencil",
            Self::PenLine => "pen-line",
            Self::MessageCircle => "message-circle",
            Self::MessageSquare => "message-square",
            Self::MessageSquareReply => "message-square-reply",
            Self::Bookmark => "bookmark",
            Self::Link => "link",
            Self::Paperclip => "paperclip",
            Self::Lock => "lock",
            Self::Menu => "menu",
            Self::Trash2 => "trash-2",
            Self::Mail => "mail",
            Self::Diamond => "diamond",
            Self::Zap => "zap",
            Self::ArrowLeftRight => "arrow-left-right",
            Self::ChartLine => "chart-line",
            Self::SmilePlus => "smile-plus",
            Self::Folder => "folder",
            Self::Package => "package",
            Self::ChevronLeft => "chevron-left",
            Self::ChevronRight => "chevron-right",
            Self::X => "x",
            Self::Ellipsis => "ellipsis",
            Self::Star => "star",
            Self::SquareArrowOutUpRight => "square-arrow-out-up-right",
            Self::ExternalLink => "external-link",
            Self::LocateFixed => "locate-fixed",
            Self::Minus => "minus",
            Self::Maximize => "maximize",
            Self::Minimize => "minimize",
            Self::Search => "search",
            Self::SlidersHorizontal => "sliders-horizontal",
            Self::Sparkles => "sparkles",
            Self::SquareCheck => "square-check",
            Self::Presentation => "presentation",
            Self::Layers => "layers",
            Self::Box => "box",
            Self::Bot => "bot",
            Self::ShieldCheck => "shield-check",
            Self::LayoutPanelLeft => "layout-panel-left",
            Self::Settings => "settings",
            Self::Globe => "globe",
            Self::Dot => "dot",
            Self::Film => "film",
            Self::Music => "music",
            Self::Sheet => "sheet",
            Self::FileArchive => "file-archive",
            Self::Image => "image",
            Self::SendHorizontal => "send-horizontal",
            Self::Server => "server",
            Self::ArrowDownToLine => "arrow-down-to-line",
            Self::ArrowDownToDot => "arrow-down-to-dot",
            Self::PanelLeftClose => "panel-left-close",
            Self::PanelLeftOpen => "panel-left-open",
            Self::Pause => "pause",
            Self::LoaderCircle => "loader-circle",
            Self::Target => "target",
            Self::Milestone => "milestone",
            Self::ChevronsLeft => "chevrons-left",
            Self::ChevronsRight => "chevrons-right",
            Self::Download => "download",
            Self::Copy => "copy",
        }
    }

    fn paths(self) -> &'static str {
        match self {
            Self::PanelsTopLeft => r#"<rect width="18" height="18" x="3" y="3" rx="2" /><path d="M3 9h18" /><path d="M9 21V9" />"#,
            Self::Archive => {
                r#"<rect width="20" height="5" x="2" y="3" rx="1" /><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" /><path d="M10 12h4" />"#
            }
            Self::ArchiveRestore => {
                r#"<rect width="20" height="5" x="2" y="3" rx="1" /><path d="M4 8v11a2 2 0 0 0 2 2h2" /><path d="M20 8v11a2 2 0 0 1-2 2h-2" /><path d="m9 15 3-3 3 3" /><path d="M12 12v9" />"#
            }
            Self::FileDiff => {
                r#"<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /><path d="M9 10h6" /><path d="M12 13V7" /><path d="M9 17h6" />"#
            }
            Self::SquareTerminal => {
                r#"<path d="m7 11 2-2-2-2" /><path d="M11 13h4" /><rect width="18" height="18" x="3" y="3" rx="2" ry="2" />"#
            }
            Self::Bell => {
                r#"<path d="M10.268 21a2 2 0 0 0 3.464 0" /><path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326" />"#
            }
            Self::BellDot => {
                r#"<path d="M10.268 21a2 2 0 0 0 3.464 0" /><path d="M11.68 2.009A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673c-.824-.85-1.678-1.731-2.21-3.348" /><circle cx="18" cy="5" r="3" />"#
            }
            Self::BellOff => {
                r#"<path d="M10.268 21a2 2 0 0 0 3.464 0" /><path d="M17 17H4a1 1 0 0 1-.74-1.673C4.59 13.956 6 12.499 6 8a6 6 0 0 1 .258-1.742" /><path d="m2 2 20 20" /><path d="M8.668 3.01A6 6 0 0 1 18 8c0 2.687.77 4.653 1.707 6.05" />"#
            }
            Self::Brain => {
                r#"<path d="M12 18V5" /><path d="M15 13a4.17 4.17 0 01-3-4 4.17 4.17 0 01-3 4" /><path d="M17.707 6.294A3 3 0 1012 5a3 3 0 10-5.707 1.294" /><path d="M17.996 5.125a4 4 0 012.526 5.77" /><path d="M18 18a4 4 0 002-7.465" /><path d="M19.967 17.482a4 4 0 11-7.967.517 4 4 0 11-7.967-.517" /><path d="M6 18a4 4 0 01-2-7.465" /><path d="M6.004 5.125a4 4 0 00-2.526 5.77" />"#
            }
            Self::LayoutGrid => {
                r#"<rect width="7" height="7" x="3" y="3" rx="1" /><rect width="7" height="7" x="14" y="3" rx="1" /><rect width="7" height="7" x="14" y="14" rx="1" /><rect width="7" height="7" x="3" y="14" rx="1" />"#
            }
            Self::Info => r#"<circle cx="12" cy="12" r="10" /><path d="M12 16v-4" /><path d="M12 8h.01" />"#,
            Self::CircleHelp => {
                r#"<circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3" /><path d="M12 17h.01" />"#
            }
            Self::User => r#"<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2" /><circle cx="12" cy="7" r="4" />"#,
            Self::Users => {
                r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><path d="M16 3.128a4 4 0 0 1 0 7.744" /><path d="M22 21v-2a4 4 0 0 0-3-3.87" /><circle cx="9" cy="7" r="4" />"#
            }
            Self::Plus => r#"<path d="M5 12h14" /><path d="M12 5v14" />"#,
            Self::Square => r#"<rect width="18" height="18" x="3" y="3" rx="2" />"#,
            Self::Quote => {
                r#"<path d="M16 3a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2 1 1 0 0 0-1 1v2a1 1 0 0 0 1 1 6 6 0 0 0 6-6V5a2 2 0 0 0-2-2z" /><path d="M5 3a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2 1 1 0 0 0-1 1v2a1 1 0 0 0 1 1 6 6 0 0 0 6-6V5a2 2 0 0 0-2-2z" />"#
            }
            Self::List => {
                r#"<path d="M3 5h.01" /><path d="M3 12h.01" /><path d="M3 19h.01" /><path d="M8 5h13" /><path d="M8 12h13" /><path d="M8 19h13" />"#
            }
            Self::ListOrdered => {
                r#"<path d="M11 5h10" /><path d="M11 12h10" /><path d="M11 19h10" /><path d="M4 4h1v5" /><path d="M4 9h2" /><path d="M6.5 20H3.4c0-1 2.6-1.925 2.6-3.5a1.5 1.5 0 0 0-2.6-1.02" />"#
            }
            Self::Calendar => {
                r#"<path d="M8 2v3" /><path d="M16 2v3" /><rect x="3" y="3" width="18" height="18" rx="2" /><path d="M3 9h18" />"#
            }
            Self::CalendarClock => {
                r#"<path d="M16 14v2.2l1.6 1" /><path d="M16 2v3" /><path d="M21 7.338V5a2 2 0 00-2-2H5a2 2 0 00-2 2v14a2 2 0 002 2h2.338" /><path d="M3 9h5.859" /><path d="M8 2v3" /><circle cx="16" cy="16" r="6" />"#
            }
            Self::LayoutTemplate => {
                r#"<rect width="18" height="7" x="3" y="3" rx="1" /><rect width="9" height="7" x="3" y="14" rx="1" /><rect width="5" height="7" x="16" y="14" rx="1" />"#
            }
            Self::File => {
                r#"<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /><path d="M14 2v5a1 1 0 0 0 1 1h5" />"#
            }
            Self::FileText => {
                r#"<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /><path d="M14 2v5a1 1 0 0 0 1 1h5" /><path d="M10 9H8" /><path d="M16 13H8" /><path d="M16 17H8" />"#
            }
            Self::KeyRound => {
                r#"<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z" /><circle cx="16.5" cy="7.5" r=".5" fill="currentColor" />"#
            }
            Self::Check => r#"<path d="M20 6 9 17l-5-5" />"#,
            Self::TriangleAlert => {
                r#"<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" /><path d="M12 9v4" /><path d="M12 17h.01" />"#
            }
            Self::Clock => r#"<circle cx="12" cy="12" r="10" /><path d="M12 6v6l4 2" />"#,
            Self::Play => r#"<path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" />"#,
            Self::Circle => r#"<circle cx="12" cy="12" r="10" />"#,
            Self::CircleDot => r#"<circle cx="12" cy="12" r="1" /><circle cx="12" cy="12" r="10" />"#,
            Self::CircleX => r#"<circle cx="12" cy="12" r="10" /><path d="m15 9-6 6" /><path d="m9 9 6 6" />"#,
            Self::CircleDashed => {
                r#"<path d="M10.1 2.182a10 10 0 0 1 3.8 0" /><path d="M13.9 21.818a10 10 0 0 1-3.8 0" /><path d="M17.609 3.721a10 10 0 0 1 2.69 2.7" /><path d="M2.182 13.9a10 10 0 0 1 0-3.8" /><path d="M20.279 17.609a10 10 0 0 1-2.7 2.69" /><path d="M21.818 10.1a10 10 0 0 1 0 3.8" /><path d="M3.721 6.391a10 10 0 0 1 2.7-2.69" /><path d="M6.391 20.279a10 10 0 0 1-2.69-2.7" />"#
            }
            Self::ChevronDown => r#"<path d="m6 9 6 6 6-6" />"#,
            Self::ChevronUp => r#"<path d="m18 15-6-6-6 6" />"#,
            Self::ArrowUp => r#"<path d="m5 12 7-7 7 7" /><path d="M12 19V5" />"#,
            Self::ArrowDown => r#"<path d="M12 5v14" /><path d="m19 12-7 7-7-7" />"#,
            Self::ArrowLeft => r#"<path d="m12 19-7-7 7-7" /><path d="M19 12H5" />"#,
            Self::ArrowRight => r#"<path d="M5 12h14" /><path d="m12 5 7 7-7 7" />"#,
            Self::ArrowRightToLine => r#"<path d="M17 12H3" /><path d="m11 18 6-6-6-6" /><path d="M21 5v14" />"#,
            Self::ArrowUpRight => r#"<path d="M7 7h10v10" /><path d="M7 17 17 7" />"#,
            Self::ArrowDownRight => r#"<path d="m7 7 10 10" /><path d="M17 7v10H7" />"#,
            Self::CornerDownRight => r#"<path d="m15 10 5 5-5 5" /><path d="M4 4v7a4 4 0 0 0 4 4h12" />"#,
            Self::Undo2 => r#"<path d="M9 14 4 9l5-5" /><path d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11" />"#,
            Self::RotateCcw => r#"<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" /><path d="M3 3v5h5" />"#,
            Self::RefreshCw => {
                r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />"#
            }
            Self::Pencil => {
                r#"<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" /><path d="m15 5 4 4" />"#
            }
            Self::PenLine => {
                r#"<path d="M13 21h8" /><path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" />"#
            }
            Self::MessageCircle => {
                r#"<path d="M2.992 16.342a2 2 0 0 1 .094 1.167l-1.065 3.29a1 1 0 0 0 1.236 1.168l3.413-.998a2 2 0 0 1 1.099.092 10 10 0 1 0-4.777-4.719" />"#
            }
            Self::MessageSquare => {
                r#"<path d="M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z" />"#
            }
            Self::MessageSquareReply => {
                r#"<path d="M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z" /><path d="m10 8-3 3 3 3" /><path d="M17 14v-1a2 2 0 0 0-2-2H7" />"#
            }
            Self::Bookmark => {
                r#"<path d="M17 3a2 2 0 0 1 2 2v15a1 1 0 0 1-1.496.868l-4.512-2.578a2 2 0 0 0-1.984 0l-4.512 2.578A1 1 0 0 1 5 20V5a2 2 0 0 1 2-2z" />"#
            }
            Self::Link => {
                r#"<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" /><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />"#
            }
            Self::Paperclip => {
                r#"<path d="m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551" />"#
            }
            Self::Lock => r#"<rect width="18" height="11" x="3" y="11" rx="2" ry="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />"#,
            Self::Menu => r#"<path d="M4 5h16" /><path d="M4 12h16" /><path d="M4 19h16" />"#,
            Self::Trash2 => {
                r#"<path d="M10 11v6" /><path d="M14 11v6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M3 6h18" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />"#
            }
            Self::Mail => r#"<path d="m22 7-8.991 5.727a2 2 0 0 1-2.009 0L2 7" /><rect x="2" y="4" width="20" height="16" rx="2" />"#,
            Self::Diamond => {
                r#"<path d="M2.7 10.3a2.41 2.41 0 0 0 0 3.41l7.59 7.59a2.41 2.41 0 0 0 3.41 0l7.59-7.59a2.41 2.41 0 0 0 0-3.41l-7.59-7.59a2.41 2.41 0 0 0-3.41 0Z" />"#
            }
            Self::Zap => {
                r#"<path d="M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z" />"#
            }
            Self::ArrowLeftRight => r#"<path d="M8 3 4 7l4 4" /><path d="M4 7h16" /><path d="m16 21 4-4-4-4" /><path d="M20 17H4" />"#,
            Self::ChartLine => r#"<path d="M3 3v16a2 2 0 0 0 2 2h16" /><path d="m19 9-5 5-4-4-3 3" />"#,
            Self::SmilePlus => {
                r#"<path d="M13.267 2.08a10 10 0 108.653 8.653" /><path d="M15 10V9" /><path d="M16 5h6" /><path d="M16.472 15a6 6 0 01-8.943 0" /><path d="M19 2v6" /><path d="M9 10V9" />"#
            }
            Self::Folder => {
                r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />"#
            }
            Self::Package => {
                r#"<path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z" /><path d="M12 22V12" /><polyline points="3.29 7 12 12 20.71 7" /><path d="m7.5 4.27 9 5.15" />"#
            }
            Self::ChevronLeft => r#"<path d="m15 18-6-6 6-6" />"#,
            Self::ChevronRight => r#"<path d="m9 18 6-6-6-6" />"#,
            Self::X => r#"<path d="M18 6 6 18" /><path d="m6 6 12 12" />"#,
            Self::Ellipsis => r#"<circle cx="12" cy="12" r="1" /><circle cx="19" cy="12" r="1" /><circle cx="5" cy="12" r="1" />"#,
            Self::Star => {
                r#"<path d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" />"#
            }
            Self::SquareArrowOutUpRight => {
                r#"<path d="M21 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h6" /><path d="m21 3-9 9" /><path d="M15 3h6v6" />"#
            }
            Self::ExternalLink => {
                r#"<path d="M15 3h6v6" /><path d="M10 14 21 3" /><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />"#
            }
            Self::LocateFixed => {
                r#"<line x1="2" x2="5" y1="12" y2="12" /><line x1="19" x2="22" y1="12" y2="12" /><line x1="12" x2="12" y1="2" y2="5" /><line x1="12" x2="12" y1="19" y2="22" /><circle cx="12" cy="12" r="7" /><circle cx="12" cy="12" r="3" />"#
            }
            Self::Minus => r#"<path d="M5 12h14" />"#,
            Self::Maximize => {
                r#"<path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" />"#
            }
            Self::Minimize => {
                r#"<path d="M8 3v3a2 2 0 0 1-2 2H3" /><path d="M21 8h-3a2 2 0 0 1-2-2V3" /><path d="M3 16h3a2 2 0 0 1 2 2v3" /><path d="M16 21v-3a2 2 0 0 1 2-2h3" />"#
            }
            Self::Search => r#"<path d="m21 21-4.34-4.34" /><circle cx="11" cy="11" r="8" />"#,
            Self::SlidersHorizontal => {
                r#"<path d="M10 5H3" /><path d="M12 19H3" /><path d="M14 3v4" /><path d="M16 17v4" /><path d="M21 12h-9" /><path d="M21 19h-5" /><path d="M21 5h-7" /><path d="M8 10v4" /><path d="M8 12H3" />"#
            }
            Self::Sparkles => {
                r#"<path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z" /><path d="M20 2v4" /><path d="M22 4h-4" /><circle cx="4" cy="20" r="2" />"#
            }
            Self::SquareCheck => r#"<rect width="18" height="18" x="3" y="3" rx="2" /><path d="m16 9-5.5 5.5L8 12" />"#,
            Self::Presentation => r#"<path d="M2 3h20" /><path d="M21 3v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V3" /><path d="m7 21 5-5 5 5" />"#,
            Self::Layers => {
                r#"<path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z" /><path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12" /><path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17" />"#
            }
            Self::Box => {
                r#"<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z" /><path d="m3.3 7 8.7 5 8.7-5" /><path d="M12 22V12" />"#
            }
            Self::Bot => {
                r#"<path d="M12 8V4H8" /><rect width="16" height="12" x="4" y="8" rx="2" /><path d="M2 14h2" /><path d="M20 14h2" /><path d="M15 13v2" /><path d="M9 13v2" />"#
            }
            Self::ShieldCheck => {
                r#"<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" /><path d="m9 12 2 2 4-4" />"#
            }
            Self::LayoutPanelLeft => {
                r#"<rect width="7" height="18" x="3" y="3" rx="1" /><rect width="7" height="7" x="14" y="3" rx="1" /><rect width="7" height="7" x="14" y="14" rx="1" />"#
            }
            Self::Settings => {
                r#"<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915" /><circle cx="12" cy="12" r="3" />"#
            }
            Self::Globe => {
                r#"<circle cx="12" cy="12" r="10" /><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" /><path d="M2 12h20" />"#
            }
            Self::Dot => r#"<circle cx="12" cy="12" r="1" />"#,
            Self::Film => {
                r#"<rect width="18" height="18" x="3" y="3" rx="2" /><path d="M7 3v18" /><path d="M3 7.5h4" /><path d="M3 12h18" /><path d="M3 16.5h4" /><path d="M17 3v18" /><path d="M17 7.5h4" /><path d="M17 16.5h4" />"#
            }
            Self::Music => {
                r#"<path d="M21 16V3a1 1 0 0 0-1.28-.96l-10 2.917a1 1 0 0 0-.72.96V19" /><circle cx="18" cy="16" r="3" /><circle cx="6" cy="19" r="3" />"#
            }
            Self::Sheet => {
                r#"<rect width="18" height="18" x="3" y="3" rx="2" ry="2" /><line x1="3" x2="21" y1="9" y2="9" /><line x1="3" x2="21" y1="15" y2="15" /><line x1="9" x2="9" y1="9" y2="21" /><line x1="15" x2="15" y1="9" y2="21" />"#
            }
            Self::FileArchive => {
                r#"<path d="M13.659 22H18a2 2 0 0 0 2-2V8a2.4 2.4 0 0 0-.706-1.706l-3.588-3.588A2.4 2.4 0 0 0 14 2H6a2 2 0 0 0-2 2v11.5" /><path d="M14 2v5a1 1 0 0 0 1 1h5" /><path d="M8 12v-1" /><path d="M8 18v-2" /><path d="M8 7V6" /><circle cx="8" cy="20" r="2" />"#
            }
            Self::Image => {
                r#"<rect width="18" height="18" x="3" y="3" rx="2" ry="2" /><circle cx="9" cy="9" r="2" /><path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21" />"#
            }
            Self::SendHorizontal => {
                r#"<path d="M3.714 3.048a.498.498 0 0 0-.683.627l2.843 7.627a2 2 0 0 1 0 1.396l-2.842 7.627a.498.498 0 0 0 .682.627l18-8.5a.5.5 0 0 0 0-.904z" /><path d="M6 12h16" />"#
            }
            Self::Server => {
                r#"<rect width="20" height="8" x="2" y="2" rx="2" ry="2" /><rect width="20" height="8" x="2" y="14" rx="2" ry="2" /><line x1="6" x2="6.01" y1="6" y2="6" /><line x1="6" x2="6.01" y1="18" y2="18" />"#
            }
            Self::ArrowDownToLine => r#"<path d="M12 17V3" /><path d="m6 11 6 6 6-6" /><path d="M19 21H5" />"#,
            Self::ArrowDownToDot => r#"<path d="M12 2v14" /><path d="m19 9-7 7-7-7" /><circle cx="12" cy="21" r="1" />"#,
            Self::PanelLeftClose => r#"<rect width="18" height="18" x="3" y="3" rx="2" /><path d="M9 3v18" /><path d="m16 15-3-3 3-3" />"#,
            Self::PanelLeftOpen => r#"<rect width="18" height="18" x="3" y="3" rx="2" /><path d="M9 3v18" /><path d="m14 9 3 3-3 3" />"#,
            Self::Pause => r#"<rect x="14" y="3" width="5" height="18" rx="1" /><rect x="5" y="3" width="5" height="18" rx="1" />"#,
            Self::LoaderCircle => r#"<path d="M21 12a9 9 0 1 1-6.219-8.56" />"#,
            Self::Target => r#"<circle cx="12" cy="12" r="10" /><circle cx="12" cy="12" r="6" /><circle cx="12" cy="12" r="2" />"#,
            Self::Milestone => {
                r#"<path d="M12 13v8" /><path d="M12 3v3" /><path d="M18.172 6a2 2 0 0 1 1.414.586l2.06 2.06a1.207 1.207 0 0 1 0 1.708l-2.06 2.06a2 2 0 0 1-1.414.586H4a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1z" />"#
            }
            Self::ChevronsLeft => r#"<path d="m11 17-5-5 5-5" /><path d="m18 17-5-5 5-5" />"#,
            Self::ChevronsRight => r#"<path d="m6 17 5-5-5-5" /><path d="m13 17 5-5-5-5" />"#,
            Self::Download => r#"<path d="M12 15V3" /><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><path d="m7 10 5 5 5-5" />"#,
            Self::Copy => {
                r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2" /><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />"#
            }
        }
    }
}

/// Decorative; the enclosing control owns its accessible name. Paths are
/// trusted, vendored data, not user HTML. No rendering a view back to a string.
/// Already an `AnyView`: callers need not erase it again.
pub fn icon(name: Icon, size: Size) -> AnyView {
    drawn(name, size, false)
}

fn drawn(name: Icon, size: Size, filled: bool) -> AnyView {
    // Share the SVG's render state across its many parent view types. Erase
    // only this view boundary, not its attributes: updates keep typed diffs.
    view! {
        <svg viewBox="0 0 24 24" class=format!("inline-block {} max-w-full shrink-0 align-text-bottom", size.class())
            width=size.pixels() height=size.pixels() fill=if filled { "currentColor" } else { "none" }
            stroke="currentColor" stroke-width=size.stroke() stroke-linecap="round" stroke-linejoin="round"
            aria-hidden="true" inner_html=name.paths()></svg>
    }
    .into_any()
}

/// The same source as `icon`, for existing HTML-string renderers. This must
/// never call `.to_html()` (the CSR build has no SSR renderer).
pub fn icon_html(name: Icon, size: Size) -> String {
    format!(
        "<svg viewBox=\"0 0 24 24\" class=\"inline-block {} max-w-full shrink-0 align-text-bottom\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">{}</svg>",
        size.class(),
        size.pixels(),
        size.pixels(),
        size.stroke(),
        name.paths()
    )
}

/// Markup for mention/document chips, never an SSR view rendered in the browser.
pub mod html {
    pub use super::icon_html as icon;
    use super::{Icon, Size};
    pub fn calendar() -> String {
        icon(Icon::Calendar, Size::S16)
    }
    pub fn file() -> String {
        icon(Icon::FileText, Size::S14)
    }
    pub fn chevron_down() -> String {
        icon(Icon::ChevronDown, Size::S14)
    }
    pub fn arrow_up_right() -> String {
        icon(Icon::ArrowUpRight, Size::S14)
    }
}

// Compatibility entry points: existing C16/C19 call sites keep their icons,
// while new code can choose a typed name and size. No second path-data system.
macro_rules! legacy_icons {
    ($($function:ident => $name:ident, $size:ident;)+) => { $(
        pub fn $function() -> AnyView { icon(Icon::$name, Size::$size) }
    )+ };
}

legacy_icons! {
    file_diff => FileDiff, S16;
    square_terminal => SquareTerminal, S16;
    bell => Bell, S16;
    bell_off => BellOff, S16;
    brain => Brain, S16;
    grid_plus => LayoutGrid, S16;
    info => Info, S16;
    help => CircleHelp, S16;
    user => User, S14;
    plus => Plus, S16;
    quote => Quote, S16;
    list => List, S16;
    list_ordered => ListOrdered, S16;
    layout_template => LayoutTemplate, S16;
    file => FileText, S16;
    key => KeyRound, S16;
    check => Check, S14;
    warning => TriangleAlert, S16;
    triangle_alert => TriangleAlert, S16;
    circle_x => CircleX, S16;
    loader_circle => LoaderCircle, S16;
    hourglass => Clock, S16;
    play => Play, S16;
    circle => CircleDot, S16;
    chevron_down => ChevronDown, S14;
    arrow_up => ArrowUp, S16;
    arrow_down => ArrowDown, S16;
    arrow_left => ArrowLeft, S16;
    arrow_right => ArrowRight, S16;
    arrow_up_right => ArrowUpRight, S14;
    corner_down_right => CornerDownRight, S16;
    undo => Undo2, S16;
    rotate => RotateCcw, S16;
    pencil => Pencil, S16;
    message => MessageCircle, S16;
    bookmark => Bookmark, S16;
    link => Link, S16;
    paperclip => Paperclip, S16;
    lock => Lock, S16;
    menu => Menu, S16;
    trash => Trash2, S16;
    mail => Mail, S16;
    diamond => Target, S16;
    chart => Presentation, S16;
    reaction => SmilePlus, S16;
    folder => Folder, S16;
    package => Package, S16;
    chevron_left => ChevronLeft, S16;
    chevron_right => ChevronRight, S16;
    x => X, S16;
    ellipsis => Ellipsis, S16;
    square_arrow_out => SquareArrowOutUpRight, S16;
    locate_fixed => LocateFixed, S16;
    minus => Minus, S16;
    maximize => Maximize, S16;
    minimize => Minimize, S16;
    search => Search, S16;
    sliders => SlidersHorizontal, S16;
    sparkles => Sparkles, S14;
}

/// Remaining C16 entry points are kept even after their last local caller
/// migrates, so an adjacent branch can still use the same icon system.
#[allow(dead_code, reason = "C16 compatibility entry point; typed call sites now choose their own size")]
pub fn arrow_down_right() -> AnyView {
    icon(Icon::ArrowDownRight, Size::S16)
}
#[allow(dead_code, reason = "C16 compatibility entry point; Explorer now selects a typed kind")]
pub fn zap() -> AnyView {
    icon(Icon::Zap, Size::S16)
}
#[allow(dead_code, reason = "C16 compatibility entry point; relationship labels now choose 14px")]
pub fn arrows_left_right() -> AnyView {
    icon(Icon::ArrowLeftRight, Size::S16)
}

/// Filled only for pinned/starred and stop states, not other chrome.
pub fn star(on: bool) -> AnyView {
    drawn(Icon::Star, Size::S16, on)
}
pub fn stop() -> AnyView {
    drawn(Icon::Square, Size::S14, true)
}
pub fn check_box(on: bool) -> AnyView {
    icon(if on { Icon::SquareCheck } else { Icon::Square }, Size::S14)
}

/// Compatibility import for bars; the component standard owns the class.
pub use crate::components::BAR_BUTTON;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_lucide_icon_uses_one_svg_and_an_absolute_stroke_at_all_sizes() {
        for &name in Icon::ALL {
            assert!(!name.paths().contains("<svg"), "{} contains an outer SVG", name.name());
            for size in [Size::S14, Size::S16, Size::S20] {
                assert!((size.stroke() * f64::from(size.pixels()) / 24.0 - 1.5).abs() < f64::EPSILON);
                let markup = icon_html(name, size);
                let view = icon(name, size).to_html();
                for html in [&markup, &view] {
                    assert_eq!(html.matches("<svg").count(), 1, "{}: {html}", name.name());
                    assert!(html.contains(name.paths()), "{} lost its paths: {html}", name.name());
                    assert!(html.contains(&format!("stroke-width=\"{}\"", size.stroke())), "{html}");
                    assert!(html.contains(&format!("width=\"{}\"", size.pixels())), "{html}");
                    assert!(html.contains("aria-hidden=\"true\""), "{html}");
                }
            }
        }
    }

    // These are text, not pictograms: punctuation, keyboard shortcuts,
    // directions in prose and the worker/SSH relationship separator.
    fn icon_glyph(c: char) -> bool {
        if "↑→↓↵⇄⇧⌘⌥─".contains(c) {
            return false;
        }
        matches!(c as u32, 0x203c | 0x2049 | 0x20e3 | 0x2100..=0x2bff | 0x3030 | 0x303d | 0x3297 | 0x3299 | 0xfe0e..=0xfe0f | 0xff0b | 0x1f000..=0x1faff)
    }

    pub(crate) fn assert_no_icon_glyphs(markup: &str) {
        let found: Vec<char> = markup.chars().filter(|c| icon_glyph(*c)).collect();
        assert!(found.is_empty(), "app-owned markup contains icon glyphs {found:?}: {markup}");
    }

    #[test]
    fn the_guard_distinguishes_chrome_icons_from_typography() {
        for c in "🔔🧠📄🔑📦⚠✓✕⏳💬★▶".chars() {
            assert!(icon_glyph(c), "missed {c}");
        }
        assert_no_icon_glyphs("Back › ‹ … · × — – “text” · ⌘/Ctrl ⇧ ↵ · Settings → Memex · worker ⇄ host");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn restored_icons_share_the_accessible_svg_grid() {
        for mark in [
            bell_off(),
            brain(),
            grid_plus(),
            info(),
            help(),
            user(),
            plus(),
            stop(),
            quote(),
            list(),
            list_ordered(),
            icon(Icon::Calendar, Size::S16),
            layout_template(),
            arrow_left(),
            arrow_right(),
            arrow_up_right(),
            icon(Icon::ArrowDownRight, Size::S16),
            corner_down_right(),
            locate_fixed(),
            undo(),
        ] {
            let html = mark.to_html();
            assert_eq!(html.matches("<svg").count(), 1, "{html}");
            for attr in ["viewBox=\"0 0 24 24\"", "stroke=\"currentColor\"", "aria-hidden=\"true\"", "stroke-linecap=\"round\""] {
                assert!(html.contains(attr), "{attr} missing: {html}");
            }
            assert_no_icon_glyphs(&html);
        }
    }

    /// The markup copies draw what the components draw.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn markup_icons_match_their_components() {
        let norm = |s: String| s.replace("<!>", "").replace("/>", ">");
        for (markup, view) in [
            (html::calendar(), icon(Icon::Calendar, Size::S16).into_any()),
            (html::file(), file().into_any()),
            (html::chevron_down(), chevron_down().into_any()),
            (html::arrow_up_right(), arrow_up_right().into_any()),
        ] {
            let drawn = view.to_html();
            for part in markup.split(['<', '>']).filter(|p| p.contains("d=\"") || p.contains("rect ")) {
                assert!(norm(drawn.clone()).contains(part.trim_end_matches('/').trim()), "{part} missing from {drawn}");
            }
            assert!(markup.starts_with("<svg viewBox=\"0 0 24 24\"") && markup.contains("aria-hidden=\"true\""), "{markup}");
        }
    }

    /// `.to_html()` panics in the browser build (no `ssr` feature), so app
    /// code builds HTML strings from `icons::html`, never from a view.
    #[test]
    fn app_code_never_renders_views_to_html() {
        fn check(path: &Path) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    check(&entry.unwrap().path());
                }
                return;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                return;
            }
            let src = std::fs::read_to_string(path).unwrap();
            let app = src.split("#[cfg(test)]").next().unwrap_or("");
            for (n, line) in app.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains(".to_html()"),
                    "{}:{}: .to_html() breaks the page in the browser; use icons::html",
                    path.display(),
                    n + 1
                );
            }
        }
        check(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    }

    #[test]
    fn app_chrome_uses_svg_not_symbol_icons() {
        fn check(path: &Path) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    check(&entry.unwrap().path());
                }
                return;
            }
            if !matches!(path.extension().and_then(|s| s.to_str()), Some("rs" | "html" | "js")) {
                return;
            }
            // Slack's emoji names/table are imported message and reaction
            // content, not icons belonging to carbon6's interface.
            if path.ends_with("chat/text.rs") {
                return;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let production = source.split("#[cfg(test)]").next().unwrap();
            for (i, line) in production.lines().enumerate() {
                if ["//", "/*", "*", "<!--"].iter().any(|prefix| line.trim_start().starts_with(prefix)) {
                    continue;
                }
                // Directions between words are prose; action arrows must
                // stay visible too, drawn as SVG rather than text glyphs.
                assert!(!line.contains("→\""), "{}:{}: draw the trailing action arrow with icons.rs", path.display(), i + 1);
                for glyph in [">\"×\"", ">\"‹\"", ">\"›\"", "\"All comments ›\""] {
                    assert!(!line.contains(glyph), "{}:{}: draw chrome action marks with icons.rs", path.display(), i + 1);
                }
                if let Some(c) = line.chars().find(|c| icon_glyph(*c)) {
                    panic!("{}:{}: {c} is an icon glyph; replace it with an icons.rs SVG, not words alone", path.display(), i + 1);
                }
            }
        }
        let ui = Path::new(env!("CARGO_MANIFEST_DIR"));
        for owned in ["src", "js", "pwa", "index.html"] {
            check(&ui.join(owned));
        }
        // vendor/xterm.js contains Unicode width/terminal data. Like
        // terminal output itself, it must still support user text.
    }
}
