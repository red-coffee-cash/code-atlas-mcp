//! Deterministic cursors for high-fanout hubs (spec §8.1).

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Caller,
    Callee,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caller => "caller",
            Self::Callee => "callee",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Caller => "callers",
            Self::Callee => "callees",
        }
    }
}

/// Wire form: `caller:offset:12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub direction: Direction,
    pub offset: usize,
}

impl Cursor {
    pub fn encode(self) -> String {
        format!("{}:offset:{}", self.direction.as_str(), self.offset)
    }

    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        let direction = match parts.first().copied() {
            Some("caller") => Direction::Caller,
            Some("callee") => Direction::Callee,
            _ => bail!("malformed neighbor_cursor `{s}`: expected `caller:offset:N` or `callee:offset:N`"),
        };
        match (parts.get(1), parts.get(2).and_then(|n| n.parse::<usize>().ok()), parts.len()) {
            (Some(&"offset"), Some(offset), 3) => Ok(Self { direction, offset }),
            _ => bail!("malformed neighbor_cursor `{s}`: expected `caller:offset:N` or `callee:offset:N`"),
        }
    }
}

/// Items `[offset, offset+limit)` plus the count left over after this page.
pub fn page<T>(items: &[T], offset: usize, limit: usize) -> (&[T], usize) {
    let start = offset.min(items.len());
    let end = (start + limit).min(items.len());
    (&items[start..end], items.len() - end)
}

/// The trailing notice from §8.1, or `None` when the page reaches the end.
pub fn omitted_notice(direction: Direction, remaining: usize, next_offset: usize) -> Option<String> {
    (remaining > 0).then(|| {
        format!(
            "[+{remaining} additional {} omitted. Call inspect_symbol_zoom with neighbor_cursor=\"{}\" to view next page.]",
            direction.plural(),
            Cursor { direction, offset: next_offset }.encode()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let c = Cursor { direction: Direction::Caller, offset: 12 };
        assert_eq!(c.encode(), "caller:offset:12");
        assert_eq!(Cursor::parse("caller:offset:12").unwrap(), c);
        assert_eq!(Cursor::parse("callee:offset:0").unwrap().direction, Direction::Callee);
        for bad in [
            "",
            "caller",
            "caller:offset",
            "caller:offset:x",
            "up:offset:1",
            "caller:off:1",
            "caller:offset:1:2",
            "caller:offset:-1",
        ] {
            assert!(Cursor::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paging_covers_everything_exactly_once() {
        let items: Vec<u32> = (0..30).collect();
        let (mut offset, mut seen) = (0, Vec::new());
        loop {
            let (chunk, remaining) = page(&items, offset, 12);
            seen.extend_from_slice(chunk);
            offset += chunk.len();
            if remaining == 0 {
                break;
            }
        }
        assert_eq!(seen, items);
        assert_eq!(page(&items, 99, 12), (&[][..], 0));
    }

    #[test]
    fn notice_matches_the_spec_wording() {
        assert_eq!(
            omitted_notice(Direction::Caller, 128, 12).unwrap(),
            "[+128 additional callers omitted. Call inspect_symbol_zoom with neighbor_cursor=\"caller:offset:12\" to view next page.]"
        );
        assert!(omitted_notice(Direction::Callee, 0, 5).is_none());
    }
}
