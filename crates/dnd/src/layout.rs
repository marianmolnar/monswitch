//! Which edges of this machine lead to another screen, from Deskflow's layout.
//!
//! Two sources:
//! - the GUI's grid (`Deskflow.conf`, `[internalConfig]`, `screens\N\name`):
//!   a 5×3 grid, N counted from 1 row by row;
//! - a server text config (`section: links`, `screen:` + `right = other`),
//!   used with `externalConfig=true`.

use crate::Edge;

/// Columns of the Deskflow GUI's screen grid.
const GRID_COLUMNS: usize = 5;

/// Edges of `me` with a neighbouring screen in the GUI grid, with the
/// neighbour's name.
pub fn from_grid(conf: &str, me: &str) -> Vec<(Edge, String)> {
    let mut cells: Vec<(usize, String)> = Vec::new();
    let mut in_section = false;
    for line in conf.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[internalConfig]");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let mut parts = key.trim().split(['\\', '/']);
        if let (Some("screens"), Some(n), Some("name")) = (parts.next(), parts.next(), parts.next())
        {
            let name = value.trim();
            if let (Ok(n), false) = (n.parse::<usize>(), name.is_empty()) {
                if n >= 1 {
                    cells.push((n - 1, name.to_string()));
                }
            }
        }
    }
    let Some(&(mine, _)) = cells.iter().find(|(_, n)| n.eq_ignore_ascii_case(me)) else {
        return Vec::new();
    };
    let (row, col) = (mine / GRID_COLUMNS, mine % GRID_COLUMNS);
    let at = |r: usize, c: usize| {
        cells
            .iter()
            .find(|(i, _)| *i == r * GRID_COLUMNS + c)
            .map(|(_, n)| n.clone())
    };
    let mut out = Vec::new();
    if col > 0 {
        out.extend(at(row, col - 1).map(|n| (Edge::Left, n)));
    }
    if col + 1 < GRID_COLUMNS {
        out.extend(at(row, col + 1).map(|n| (Edge::Right, n)));
    }
    if row > 0 {
        out.extend(at(row - 1, col).map(|n| (Edge::Top, n)));
    }
    out.extend(at(row + 1, col).map(|n| (Edge::Bottom, n)));
    out
}

/// Edges of `me` from a Deskflow server text config's `section: links`.
pub fn from_links(conf: &str, me: &str) -> Vec<(Edge, String)> {
    let mut out = Vec::new();
    let mut in_links = false;
    let mut current: Option<String> = None;
    for raw in conf.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix("section:") {
            in_links = name.trim().eq_ignore_ascii_case("links");
            current = None;
            continue;
        }
        if line == "end" {
            in_links = false;
            continue;
        }
        if !in_links {
            continue;
        }
        if let Some(screen) = line.strip_suffix(':') {
            current = Some(screen.trim().to_string());
            continue;
        }
        let (Some(screen), Some((dir, target))) = (&current, line.split_once('=')) else {
            continue;
        };
        if !screen.eq_ignore_ascii_case(me) {
            continue;
        }
        // "right(0,50) = other(0,50)": ranges are ignored
        let dir = dir.split('(').next().unwrap_or("");
        let target = target.split('(').next().unwrap_or("").trim();
        if let (Some(edge), false) = (Edge::parse(dir), target.is_empty()) {
            out.push((edge, target.to_string()));
        }
    }
    out
}

/// This machine's screen name in `Deskflow.conf` (`[core]` `computerName`).
pub fn computer_name(conf: &str) -> Option<String> {
    let mut in_core = false;
    for line in conf.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
            continue;
        }
        if let (true, Some((k, v))) = (in_core, line.split_once('=')) {
            if k.trim() == "computerName" && !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUI: &str = r"[internalConfig]
screens\1\name=
screens\7\name=
screens\8\name=DESKTOP-89H3E11
screens\9\name=MarianMac
screens\3\name=Laptop
screens\size=15

[core]
computerName=DESKTOP-89H3E11
";

    #[test]
    fn grid_neighbours() {
        assert_eq!(
            from_grid(GUI, "desktop-89h3e11"),
            vec![
                (Edge::Right, "MarianMac".to_string()),
                (Edge::Top, "Laptop".to_string())
            ]
        );
        assert_eq!(
            from_grid(GUI, "MarianMac"),
            vec![(Edge::Left, "DESKTOP-89H3E11".to_string())]
        );
        assert!(from_grid(GUI, "Nobody").is_empty());
        assert_eq!(computer_name(GUI).as_deref(), Some("DESKTOP-89H3E11"));
    }

    #[test]
    fn grid_does_not_wrap_rows() {
        // 5 is the last column of row 0, 6 the first of row 1
        let conf = "[internalConfig]\nscreens\\5\\name=A\nscreens\\6\\name=B\n";
        assert!(from_grid(conf, "A").is_empty());
        assert!(from_grid(conf, "B").is_empty());
    }

    #[test]
    fn links_section() {
        let conf = "section: screens\n\tpc:\n\tmac:\nend\nsection: links\n\tpc:\n\t\tright(0,100) = mac # PbP\n\tmac:\n\t\tleft = pc\nend\n";
        assert_eq!(
            from_links(conf, "PC"),
            vec![(Edge::Right, "mac".to_string())]
        );
        assert_eq!(
            from_links(conf, "mac"),
            vec![(Edge::Left, "pc".to_string())]
        );
    }
}
