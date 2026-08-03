//! Scrollback search data and pure matching/navigation logic.

use cmux::{RenderRow, TerminalId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: u64,
    /// Character offsets in the original, unfolded line.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub generation: u64,
    pub terminal: TerminalId,
    pub query: String,
    pub expected_history_rows: u32,
    pub viewport_offset: u64,
    pub viewport: Vec<String>,
    pub refresh: bool,
}

#[derive(Clone, Debug)]
pub struct SearchResults {
    pub generation: u64,
    pub terminal: TerminalId,
    pub query: String,
    pub history_rows: u64,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Default)]
pub struct SearchUiState {
    pub open: bool,
    pub generation: u64,
    pub terminal: Option<TerminalId>,
    pub query: String,
    pub history_rows: u64,
    pub matches: Vec<SearchMatch>,
    pub selected: Option<usize>,
}

impl SearchUiState {
    pub fn begin(&mut self, terminal: TerminalId) {
        self.open = true;
        self.terminal = Some(terminal);
        self.query.clear();
        self.history_rows = 0;
        self.matches.clear();
        self.selected = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.terminal = None;
        self.query.clear();
        self.history_rows = 0;
        self.matches.clear();
        self.selected = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn accept(&mut self, results: SearchResults) -> bool {
        if !self.open
            || self.generation != results.generation
            || self.terminal.as_ref() != Some(&results.terminal)
            || self.query != results.query
        {
            return false;
        }
        let previous = self
            .selected
            .and_then(|index| self.matches.get(index))
            .cloned();
        self.history_rows = results.history_rows;
        self.matches = results.matches;
        self.selected = previous
            .and_then(|selected| {
                self.matches
                    .iter()
                    .position(|candidate| candidate == &selected)
            })
            .or_else(|| (!self.matches.is_empty()).then(|| self.matches.len() - 1));
        true
    }

    pub fn navigate(&mut self, upward: bool) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            self.selected = None;
            return None;
        }
        let length = self.matches.len();
        self.selected = Some(match self.selected {
            Some(index) if upward => (index + length - 1) % length,
            Some(index) => (index + 1) % length,
            None if upward => length - 1,
            None => 0,
        });
        self.selected.and_then(|index| self.matches.get(index))
    }

    pub fn status(&self) -> String {
        match self.selected {
            Some(index) => format!("{}/{}", index + 1, self.matches.len()),
            None => format!("0/{}", self.matches.len()),
        }
    }

    pub fn visible_query(&self, terminal: &TerminalId) -> Option<&str> {
        (self.open && self.terminal.as_ref() == Some(terminal) && !self.query.is_empty())
            .then_some(self.query.as_str())
    }
}

pub fn row_text(row: &RenderRow) -> String {
    row.runs.iter().map(|run| run.text.as_str()).collect()
}

/// Append only viewport rows newer than retained history. Rows visible while
/// scrolled back already exist in `history` and must not be duplicated.
pub fn complete_document(
    history: &[String],
    viewport: &[String],
    viewport_offset: u64,
) -> Vec<String> {
    let mut lines = history.to_vec();
    let history_len = history.len() as u64;
    for (row, line) in viewport.iter().enumerate() {
        if viewport_offset.saturating_add(row as u64) >= history_len {
            lines.push(line.clone());
        }
    }
    lines
}

pub fn find_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(line, text)| {
            find_line_matches(text, query)
                .into_iter()
                .map(move |(start, end)| SearchMatch {
                    line: line as u64,
                    start,
                    end,
                })
        })
        .collect()
}

pub fn find_line_matches(line: &str, query: &str) -> Vec<(usize, usize)> {
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    if query.is_empty() {
        return Vec::new();
    }
    let mut folded = Vec::new();
    let mut original_columns = Vec::new();
    for (column, character) in line.chars().enumerate() {
        for folded_character in character.to_lowercase() {
            folded.push(folded_character);
            original_columns.push(column);
        }
    }
    if query.len() > folded.len() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for start in 0..=folded.len() - query.len() {
        if folded[start..start + query.len()] != query {
            continue;
        }
        let range = (
            original_columns[start],
            original_columns[start + query.len() - 1] + 1,
        );
        if matches.last() != Some(&range) {
            matches.push(range);
        }
    }
    matches
}

/// Delta for `terminal.viewport.scroll`, whose negative direction moves up.
pub fn scroll_delta_to_line(
    line: u64,
    history_rows: u64,
    viewport_rows: u16,
    viewport_offset: u64,
) -> i32 {
    let center = u64::from(viewport_rows) / 2;
    let desired = line.saturating_sub(center).min(history_rows);
    let delta = i128::from(desired) - i128::from(viewport_offset.min(history_rows));
    delta.clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literal_case_insensitive_matches_across_cached_lines() {
        let lines = vec![
            "Alpha beta ALPHA".to_string(),
            "nothing".to_string(),
            "alphabet".to_string(),
            "[alpha]".to_string(),
        ];
        assert_eq!(
            find_matches(&lines, "aLpHa"),
            vec![
                SearchMatch {
                    line: 0,
                    start: 0,
                    end: 5,
                },
                SearchMatch {
                    line: 0,
                    start: 11,
                    end: 16,
                },
                SearchMatch {
                    line: 2,
                    start: 0,
                    end: 5,
                },
                SearchMatch {
                    line: 3,
                    start: 1,
                    end: 6,
                },
            ]
        );
        assert!(find_matches(&lines, "^alpha$").is_empty());
        assert_eq!(find_line_matches("CAFÉ café", "CafÉ"), vec![(0, 4), (5, 9)]);
    }

    #[test]
    fn viewport_completion_avoids_scrollback_duplicates() {
        let history = (0..5).map(|row| format!("h{row}")).collect::<Vec<_>>();
        let viewport = vec!["h3".into(), "h4".into(), "live0".into(), "live1".into()];
        assert_eq!(
            complete_document(&history, &viewport, 3),
            vec!["h0", "h1", "h2", "h3", "h4", "live0", "live1"]
        );
    }

    #[test]
    fn match_navigation_delta_centers_and_clamps_document_lines() {
        assert_eq!(scroll_delta_to_line(10, 100, 20, 100), -100);
        assert_eq!(scroll_delta_to_line(60, 100, 20, 20), 30);
        assert_eq!(scroll_delta_to_line(119, 100, 20, 50), 50);
    }

    #[test]
    fn refreshed_results_preserve_the_selected_match() {
        let terminal = TerminalId::parse(format!("term_{:032x}", 1)).unwrap();
        let first = SearchMatch {
            line: 2,
            start: 1,
            end: 4,
        };
        let second = SearchMatch {
            line: 8,
            start: 0,
            end: 3,
        };
        let mut state = SearchUiState::default();
        state.begin(terminal.clone());
        state.query = "hit".to_string();
        state.generation = 1;
        assert!(state.accept(SearchResults {
            generation: 1,
            terminal: terminal.clone(),
            query: "hit".to_string(),
            history_rows: 10,
            matches: vec![first.clone(), second.clone()],
        }));
        assert_eq!(state.selected, Some(1));
        assert_eq!(state.navigate(true), Some(&first));

        state.generation = 2;
        assert!(state.accept(SearchResults {
            generation: 2,
            terminal,
            query: "hit".to_string(),
            history_rows: 11,
            matches: vec![first, second],
        }));
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.status(), "1/2");
    }
}
