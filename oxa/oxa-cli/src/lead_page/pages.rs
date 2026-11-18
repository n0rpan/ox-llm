use std::fmt::Write;

pub(super) const BOARD_ROWS: usize = 50;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    Status,
    Workers,
    Attention,
}

/// Read-only destinations; request text never becomes a renderer instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Asks,
    Board,
    BoardPage(usize),
    Workers,
    Usage,
    Chat(Prompt),
}

impl Page {
    const ALL: [Self; 5] = [Self::Home, Self::Asks, Self::Board, Self::Workers, Self::Usage];

    pub fn title(self) -> &'static str {
        match self {
            Self::Home => "Status",
            Self::Asks => "Asks",
            Self::Board | Self::BoardPage(_) => "Board",
            Self::Workers => "Workers",
            Self::Usage => "Usage",
            Self::Chat(_) => "Chat with lead",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Home => "/",
            Self::Asks => "/asks",
            Self::Board | Self::BoardPage(_) => "/board",
            Self::Workers => "/workers",
            Self::Usage => "/usage",
            Self::Chat(_) => "/chat",
        }
    }

    pub(crate) fn request(path: &str) -> Option<(Self, bool)> {
        let (path, fragment) = path
            .strip_suffix("?fragment=1")
            .or_else(|| path.strip_suffix("&fragment=1"))
            .map_or((path, false), |path| (path, true));
        if let Some(number) = path.strip_prefix("/board?page=") {
            let page: usize = number.parse().ok()?;
            return (page > 0 && page.checked_mul(BOARD_ROWS).is_some() && page.to_string() == number)
                .then_some((Self::BoardPage(page), fragment));
        }
        let prompt = match path {
            "/chat" | "/chat?prompt=status" => Some(Prompt::Status),
            "/chat?prompt=workers" => Some(Prompt::Workers),
            "/chat?prompt=attention" => Some(Prompt::Attention),
            _ => None,
        };
        if let Some(prompt) = prompt {
            return Some((Self::Chat(prompt), fragment));
        }
        Self::ALL
            .into_iter()
            .find(|page| page.path() == path)
            .map(|page| (page, fragment))
    }

    pub(super) fn navigation(self) -> String {
        let mut html = String::from("<nav class=tabs aria-label=Main>");
        let selected = if matches!(self, Self::BoardPage(_)) {
            Self::Board
        } else {
            self
        };
        for page in Self::ALL {
            let _ = write!(
                html,
                "<a href={}{}>{}</a>",
                page.path(),
                if page == selected {
                    " aria-current=page"
                } else {
                    ""
                },
                if page == Self::Home { "Home" } else { page.title() }
            );
        }
        let _ = write!(
            html,
            "<a class=compose href=/chat aria-label=\"Chat with lead\"{}>✎ Chat</a></nav>",
            if matches!(self, Self::Chat(_)) {
                " aria-current=page"
            } else {
                ""
            }
        );
        html
    }
}
