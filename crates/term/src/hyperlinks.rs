use alacritty_terminal::{
    Term,
    event::EventListener,
    grid::Dimensions,
    index::{Boundary, Column, Direction, Line, Point},
    term::search::{RegexIter, RegexSearch},
};

const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://|zed://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>\"\s{-}\^⟨⟩`']+"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyperlinkMatch {
    pub url: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

pub(crate) fn find<T: EventListener>(
    term: &Term<T>,
    row: usize,
    col: usize,
) -> Option<HyperlinkMatch> {
    let offset = term.grid().display_offset() as i32;
    let point = Point::new(
        Line(row as i32 - offset),
        Column(col.min(term.columns() - 1)),
    );
    let grid = term.grid();

    let (url, start, end) = if let Some(link) = grid[point].hyperlink() {
        let mut start = point;
        loop {
            let previous = start.sub(term, Boundary::Cursor, 1);
            if previous == start || grid[previous].hyperlink().as_ref() != Some(&link) {
                break;
            }
            start = previous;
        }
        let mut end = point;
        loop {
            let next = end.add(term, Boundary::Cursor, 1);
            if next == end || grid[next].hyperlink().as_ref() != Some(&link) {
                break;
            }
            end = next;
        }
        (link.uri().to_owned(), start, end)
    } else {
        let left = term.line_search_left(point);
        let right = term.line_search_right(point);
        let mut regex = RegexSearch::new(URL_REGEX).ok()?;
        let matched = RegexIter::new(left, right, Direction::Right, term, &mut regex)
            .find(|matched| matched.contains(&point))?;
        let start = *matched.start();
        let mut end = *matched.end();
        let mut url = term.bounds_to_string(start, end);
        while url.ends_with(['.', ',', ':', ';', '!', '?']) {
            url.pop();
            if end.column.0 > 0 {
                end.column -= 1;
            }
        }
        (url, start, end)
    };
    Some(HyperlinkMatch {
        url,
        start: ((start.line.0 + offset) as usize, start.column.0),
        end: ((end.line.0 + offset) as usize, end.column.0),
    })
}
