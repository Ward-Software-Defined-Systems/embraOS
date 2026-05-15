//! Menu-bar data model for embra-desktop.
//!
//! Menus and items live as `&'static [MenuItem]` tables so the structure
//! is declarative and allocation-free. Activation produces an `Action`
//! that `update` dispatches: `Direct` → `dispatch_slash`; `Prompt` →
//! open modal; `OpenSubmenu` → reveal nested column; `Quit` →
//! `iced::exit()`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPanel {
    File,
    View,
    Provider,
    Settings,
    Setup,
    Help,
}

impl MenuPanel {
    pub fn label(self) -> &'static str {
        match self {
            MenuPanel::File => "File",
            MenuPanel::View => "View",
            MenuPanel::Provider => "Provider",
            MenuPanel::Settings => "Settings",
            MenuPanel::Setup => "Setup",
            MenuPanel::Help => "Help",
        }
    }

    pub fn items(self) -> &'static [MenuItem] {
        match self {
            MenuPanel::File => FILE_MENU,
            MenuPanel::View => VIEW_MENU,
            MenuPanel::Provider => PROVIDER_MENU,
            MenuPanel::Settings => SETTINGS_MENU,
            MenuPanel::Setup => SETUP_MENU,
            MenuPanel::Help => HELP_MENU,
        }
    }

    pub const ALL: &'static [MenuPanel] = &[
        MenuPanel::File,
        MenuPanel::View,
        MenuPanel::Provider,
        MenuPanel::Settings,
        MenuPanel::Setup,
        MenuPanel::Help,
    ];
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Direct {
        command: &'static str,
        args: &'static str,
    },
    Prompt {
        title: &'static str,
        command: &'static str,
    },
    OpenSubmenu(&'static [MenuItem]),
    Quit,
}

#[derive(Debug, Clone, Copy)]
pub enum MenuItem {
    Action {
        label: &'static str,
        action: Action,
    },
    Separator,
}

#[derive(Debug, Clone, Copy)]
pub enum NavDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Default, Clone)]
pub struct MenuState {
    pub open: Option<MenuPanel>,
    pub selected: usize,
    pub submenu_open: bool,
    pub submenu_selected: usize,
}

impl MenuState {
    pub fn open_panel(panel: MenuPanel) -> Self {
        Self {
            open: Some(panel),
            selected: first_action_index(panel.items()),
            submenu_open: false,
            submenu_selected: 0,
        }
    }

    pub fn navigate(&mut self, dir: NavDir) {
        let Some(panel) = self.open else {
            return;
        };
        let items = panel.items();
        match dir {
            NavDir::Up => {
                if self.submenu_open {
                    if let Some(sub) = current_submenu(items, self.selected) {
                        self.submenu_selected = prev_action_index(sub, self.submenu_selected);
                    }
                } else {
                    self.selected = prev_action_index(items, self.selected);
                }
            }
            NavDir::Down => {
                if self.submenu_open {
                    if let Some(sub) = current_submenu(items, self.selected) {
                        self.submenu_selected = next_action_index(sub, self.submenu_selected);
                    }
                } else {
                    self.selected = next_action_index(items, self.selected);
                }
            }
            NavDir::Right => {
                if !self.submenu_open {
                    if let Some(sub) = current_submenu(items, self.selected) {
                        self.submenu_open = true;
                        self.submenu_selected = first_action_index(sub);
                        return;
                    }
                    // No submenu — walk to the next top-level panel.
                    let next = next_panel(panel);
                    *self = MenuState::open_panel(next);
                }
            }
            NavDir::Left => {
                if self.submenu_open {
                    self.submenu_open = false;
                    self.submenu_selected = 0;
                } else {
                    let prev = prev_panel(panel);
                    *self = MenuState::open_panel(prev);
                }
            }
        }
    }

    /// Item the user would activate right now (parent or submenu leaf).
    pub fn active_action(&self) -> Option<Action> {
        let panel = self.open?;
        let items = panel.items();
        if self.submenu_open {
            let sub = current_submenu(items, self.selected)?;
            match *sub.get(self.submenu_selected)? {
                MenuItem::Action { action, .. } => Some(action),
                MenuItem::Separator => None,
            }
        } else {
            match *items.get(self.selected)? {
                MenuItem::Action { action, .. } => Some(action),
                MenuItem::Separator => None,
            }
        }
    }
}

fn current_submenu(items: &'static [MenuItem], i: usize) -> Option<&'static [MenuItem]> {
    match items.get(i)? {
        MenuItem::Action {
            action: Action::OpenSubmenu(sub),
            ..
        } => Some(sub),
        _ => None,
    }
}

fn first_action_index(items: &[MenuItem]) -> usize {
    items
        .iter()
        .position(|i| matches!(i, MenuItem::Action { .. }))
        .unwrap_or(0)
}

fn next_action_index(items: &[MenuItem], current: usize) -> usize {
    let n = items.len();
    if n == 0 {
        return 0;
    }
    let mut idx = (current + 1) % n;
    for _ in 0..n {
        if matches!(items[idx], MenuItem::Action { .. }) {
            return idx;
        }
        idx = (idx + 1) % n;
    }
    current
}

fn prev_action_index(items: &[MenuItem], current: usize) -> usize {
    let n = items.len();
    if n == 0 {
        return 0;
    }
    let mut idx = if current == 0 { n - 1 } else { current - 1 };
    for _ in 0..n {
        if matches!(items[idx], MenuItem::Action { .. }) {
            return idx;
        }
        idx = if idx == 0 { n - 1 } else { idx - 1 };
    }
    current
}

fn next_panel(p: MenuPanel) -> MenuPanel {
    let all = MenuPanel::ALL;
    let i = all.iter().position(|&x| x == p).unwrap_or(0);
    all[(i + 1) % all.len()]
}

fn prev_panel(p: MenuPanel) -> MenuPanel {
    let all = MenuPanel::ALL;
    let i = all.iter().position(|&x| x == p).unwrap_or(0);
    all[(i + all.len() - 1) % all.len()]
}

#[derive(Debug, Clone)]
pub struct ModalState {
    pub title: String,
    pub pending_command: String,
    pub input: String,
}

// === Menu definitions ===

const FILE_MENU: &[MenuItem] = &[
    MenuItem::Action {
        label: "New Session…",
        action: Action::Prompt {
            title: "Session name",
            command: "/new",
        },
    },
    MenuItem::Action {
        label: "Switch Session…",
        action: Action::Prompt {
            title: "Session name",
            command: "/switch",
        },
    },
    MenuItem::Action {
        label: "Close Session",
        action: Action::Direct {
            command: "/close",
            args: "",
        },
    },
    MenuItem::Action {
        label: "List Sessions",
        action: Action::Direct {
            command: "/sessions",
            args: "",
        },
    },
    MenuItem::Separator,
    MenuItem::Action {
        label: "Quit",
        action: Action::Quit,
    },
];

const VIEW_MENU: &[MenuItem] = &[
    MenuItem::Action {
        label: "Status",
        action: Action::Direct {
            command: "/status",
            args: "",
        },
    },
    MenuItem::Action {
        label: "Mode",
        action: Action::Direct {
            command: "/mode",
            args: "",
        },
    },
    MenuItem::Action {
        label: "Identity",
        action: Action::Direct {
            command: "/identity",
            args: "",
        },
    },
    MenuItem::Action {
        label: "Soul Document",
        action: Action::Direct {
            command: "/soul",
            args: "",
        },
    },
    MenuItem::Separator,
    MenuItem::Action {
        label: "Reasoning: On",
        action: Action::Direct {
            command: "/show-reasoning",
            args: "on",
        },
    },
    MenuItem::Action {
        label: "Reasoning: Off",
        action: Action::Direct {
            command: "/show-reasoning",
            args: "off",
        },
    },
    MenuItem::Action {
        label: "Reasoning: Reset",
        action: Action::Direct {
            command: "/show-reasoning",
            args: "reset",
        },
    },
    MenuItem::Action {
        label: "Reasoning: Show",
        action: Action::Direct {
            command: "/show-reasoning",
            args: "",
        },
    },
];

const PROVIDER_SWITCH_SUB: &[MenuItem] = &[
    MenuItem::Action {
        label: "Anthropic",
        action: Action::Direct {
            command: "/provider",
            args: "anthropic",
        },
    },
    MenuItem::Action {
        label: "Gemini",
        action: Action::Direct {
            command: "/provider",
            args: "gemini",
        },
    },
    MenuItem::Action {
        label: "Ollama",
        action: Action::Direct {
            command: "/provider",
            args: "ollama",
        },
    },
    MenuItem::Action {
        label: "LM Studio",
        action: Action::Direct {
            command: "/provider",
            args: "lm_studio",
        },
    },
];

const PROVIDER_SETUP_SUB: &[MenuItem] = &[
    MenuItem::Action {
        label: "Anthropic…",
        action: Action::Direct {
            command: "/provider",
            args: "--setup anthropic",
        },
    },
    MenuItem::Action {
        label: "Gemini…",
        action: Action::Direct {
            command: "/provider",
            args: "--setup gemini",
        },
    },
    MenuItem::Action {
        label: "Ollama…",
        action: Action::Direct {
            command: "/provider",
            args: "--setup ollama",
        },
    },
    MenuItem::Action {
        label: "LM Studio…",
        action: Action::Direct {
            command: "/provider",
            args: "--setup lm_studio",
        },
    },
];

const PROVIDER_MENU: &[MenuItem] = &[
    MenuItem::Action {
        label: "Show Active",
        action: Action::Direct {
            command: "/provider",
            args: "",
        },
    },
    MenuItem::Action {
        label: "Switch  ▸",
        action: Action::OpenSubmenu(PROVIDER_SWITCH_SUB),
    },
    MenuItem::Action {
        label: "Set Up  ▸",
        action: Action::OpenSubmenu(PROVIDER_SETUP_SUB),
    },
];

const SETTINGS_MENU: &[MenuItem] = &[
    MenuItem::Action {
        label: "Iteration Cap…",
        action: Action::Prompt {
            title: "Iteration cap (1-1000)",
            command: "/iter-cap",
        },
    },
    MenuItem::Action {
        label: "Iteration Cap: Show",
        action: Action::Direct {
            command: "/iter-cap",
            args: "",
        },
    },
    MenuItem::Action {
        label: "Iteration Cap: Reset",
        action: Action::Direct {
            command: "/iter-cap",
            args: "reset",
        },
    },
    MenuItem::Separator,
    MenuItem::Action {
        label: "Feedback Loop (experimental)",
        action: Action::Direct {
            command: "/feedback-loop",
            args: "",
        },
    },
];

const SETUP_MENU: &[MenuItem] = &[
    MenuItem::Action {
        label: "GitHub Token…",
        action: Action::Prompt {
            title: "GitHub token",
            command: "/github-token",
        },
    },
    MenuItem::Action {
        label: "SSH Keygen",
        action: Action::Direct {
            command: "/ssh-keygen",
            args: "",
        },
    },
    MenuItem::Action {
        label: "SSH Copy ID…",
        action: Action::Prompt {
            title: "user@host (RFC 1918)",
            command: "/ssh-copy-id",
        },
    },
    MenuItem::Separator,
    MenuItem::Action {
        label: "Git Setup…",
        action: Action::Prompt {
            title: "Name | Email",
            command: "/git-setup",
        },
    },
    MenuItem::Action {
        label: "Git Setup: Show",
        action: Action::Direct {
            command: "/git-setup",
            args: "",
        },
    },
];

const HELP_MENU: &[MenuItem] = &[MenuItem::Action {
    label: "Help",
    action: Action::Direct {
        command: "/help",
        args: "",
    },
}];
