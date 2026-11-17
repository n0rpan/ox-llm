use std::fmt::Write;

/// Read-only destinations; request text never becomes a renderer instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Asks,
    Board,
    Workers,
    Usage,
    Item(u64),
}

impl Page {
    const ALL: [Self; 5] = [Self::Home, Self::Asks, Self::Board, Self::Workers, Self::Usage];

    pub fn title(self) -> &'static str {
        match self {
            Self::Home => "Status",
            Self::Asks => "Asks",
            Self::Board => "Board",
            Self::Workers => "Workers",
            Self::Usage => "Usage",
            Self::Item(_) => "Item",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Home => "/",
            Self::Asks => "/asks",
            Self::Board | Self::Item(_) => "/board",
            Self::Workers => "/workers",
            Self::Usage => "/usage",
        }
    }

    pub(crate) fn request(path: &str) -> Option<(Self, bool)> {
        let (path, fragment) = path
            .strip_suffix("?fragment=1")
            .map_or((path, false), |path| (path, true));
        if let Some(number) = path.strip_prefix("/items/") {
            let parsed: u64 = number.parse().ok()?;
            return (parsed > 0 && parsed.to_string() == number).then_some((Self::Item(parsed), fragment));
        }
        Self::ALL
            .into_iter()
            .find(|page| page.path() == path)
            .map(|page| (page, fragment))
    }

    pub(super) fn navigation(self) -> String {
        let mut html = String::from("<nav class=tabs aria-label=Main>");
        let selected = if matches!(self, Self::Item(_)) {
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
        html.push_str("</nav>");
        html
    }
}
