//! A cheap rejection test for sticky matching. `regress` (with UTF-16 support) tries every position up to the end of
//! the input even when only one position can match, so a failing sticky `exec` would cost a scan of the rest of the
//! text. For patterns that cannot match the empty string, every match starts with one of a few single-character atoms
//! (a literal, a class, an escape, `.`); [`first_atoms`] collects their source text so a tiny probe regex built from
//! them can test the one character at `lastIndex` first. Anything the walk does not fully understand gives up, which
//! only means no shortcut.

/// Source slices of the single-character atoms some match must start with, or `None` when unknown (or when the
/// pattern can match the empty string).
pub fn first_atoms(chars: &[u32], unicode: bool) -> Option<Vec<Vec<u32>>> {
    let mut walker = Walker { s: chars, i: 0, unicode };
    let (atoms, nullable) = walker.disjunction()?;
    if nullable || walker.i != chars.len() || atoms.is_empty() {
        return None;
    }
    Some(atoms)
}

enum Term {
    /// First atoms and whether the term can match empty.
    Known(Vec<Vec<u32>>, bool),
    /// A term whose first character is unknown.
    Unknown,
}

struct Walker<'a> {
    s: &'a [u32],
    i: usize,
    unicode: bool,
}

fn c(ch: char) -> u32 {
    ch as u32
}

fn is_digit(u: u32) -> bool {
    (c('0')..=c('9')).contains(&u)
}

fn is_hex(u: u32) -> bool {
    char::from_u32(u).is_some_and(|x| x.is_ascii_hexdigit())
}

impl Walker<'_> {
    fn peek(&self) -> Option<u32> {
        self.s.get(self.i).copied()
    }
    fn peek_at(&self, k: usize) -> Option<u32> {
        self.s.get(self.i + k).copied()
    }

    /// Alternatives: the union of their first atoms; nullable when any alternative is.
    fn disjunction(&mut self) -> Option<(Vec<Vec<u32>>, bool)> {
        let mut atoms = Vec::new();
        let mut nullable = false;
        loop {
            let (a, n) = self.sequence()?;
            atoms.extend(a);
            nullable |= n;
            if self.peek() == Some(c('|')) {
                self.i += 1;
                continue;
            }
            return Some((atoms, nullable));
        }
    }

    fn sequence(&mut self) -> Option<(Vec<Vec<u32>>, bool)> {
        let mut atoms = Vec::new();
        let mut done = false;
        while let Some(u) = self.peek() {
            if u == c('|') || u == c(')') {
                break;
            }
            let (a, nullable) = match self.term()? {
                Term::Known(a, nullable) => (a, nullable),
                Term::Unknown if done => continue,
                Term::Unknown => return None,
            };
            if !done {
                atoms.extend(a);
                done = !nullable;
            }
        }
        Some((atoms, !done))
    }

    /// One atom and its quantifier.
    fn term(&mut self) -> Option<Term> {
        let start = self.i;
        let u = self.peek()?;
        let (atoms, mut nullable, quantifiable) = match char::from_u32(u) {
            Some('^') | Some('$') => {
                self.i += 1;
                (Vec::new(), true, false)
            }
            Some('(') => self.group()?,
            Some('[') => {
                self.class()?;
                (vec![self.s[start..self.i].to_vec()], false, true)
            }
            Some('\\') => {
                let next = self.peek_at(1)?;
                if next == c('b') || next == c('B') {
                    self.i += 2;
                    (Vec::new(), true, false)
                } else if is_digit(next) && next != c('0') || next == c('k') {
                    // A backreference (or an Annex B octal or identity escape): what it matches is unknown here.
                    self.i += 2;
                    if next == c('k') && self.peek() == Some(c('<')) {
                        while self.peek()? != c('>') {
                            self.i += 1;
                        }
                        self.i += 1;
                    }
                    while self.peek().is_some_and(is_digit) {
                        self.i += 1;
                    }
                    self.quantifier()?;
                    return Some(Term::Unknown);
                } else {
                    self.escape()?;
                    (vec![self.s[start..self.i].to_vec()], false, true)
                }
            }
            Some('*') | Some('+') | Some('?') | Some('{') | Some(']') | Some('}') => return None,
            _ => {
                self.i += 1;
                (vec![self.s[start..self.i].to_vec()], false, true)
            }
        };
        if let Some(zero) = self.quantifier()? {
            if !quantifiable && !atoms.is_empty() {
                return None;
            }
            nullable |= zero;
        }
        Some(Term::Known(atoms, nullable))
    }

    /// An optional quantifier: `Some(min_is_zero)` when there is one.
    fn quantifier(&mut self) -> Option<Option<bool>> {
        if let Some(q) = self.peek() {
            let min_zero = match char::from_u32(q) {
                Some('*') | Some('?') => {
                    self.i += 1;
                    Some(true)
                }
                Some('+') => {
                    self.i += 1;
                    Some(false)
                }
                Some('{') => Some(self.braces()?),
                _ => None,
            };
            if min_zero.is_some() && self.peek() == Some(c('?')) {
                self.i += 1;
            }
            return Some(min_zero);
        }
        Some(None)
    }

    /// `{n}`, `{n,}`, `{n,m}`: whether the minimum is zero. Anything else (an Annex B literal brace) gives up.
    fn braces(&mut self) -> Option<bool> {
        self.i += 1;
        let digits = |w: &mut Walker| {
            let from = w.i;
            let mut n: u64 = 0;
            while let Some(d) = w.peek().filter(|&d| is_digit(d)) {
                n = n.saturating_mul(10).saturating_add((d - c('0')) as u64);
                w.i += 1;
            }
            (w.i > from).then_some(n)
        };
        let min = digits(self)?;
        if self.peek() == Some(c(',')) {
            self.i += 1;
            digits(self);
        }
        if self.peek() != Some(c('}')) {
            return None;
        }
        self.i += 1;
        Some(min == 0)
    }

    /// A group: lookarounds are zero-width; capturing and non-capturing groups pass their contents' first atoms.
    fn group(&mut self) -> Option<(Vec<Vec<u32>>, bool, bool)> {
        self.i += 1;
        let mut lookaround = false;
        if self.peek() == Some(c('?')) {
            match (self.peek_at(1).and_then(char::from_u32), self.peek_at(2).and_then(char::from_u32)) {
                (Some(':'), _) => self.i += 2,
                (Some('=') | Some('!'), _) => {
                    self.i += 2;
                    lookaround = true;
                }
                (Some('<'), Some('=') | Some('!')) => {
                    self.i += 3;
                    lookaround = true;
                }
                (Some('<'), _) => {
                    self.i += 2;
                    while self.peek()? != c('>') {
                        self.i += 1;
                    }
                    self.i += 1;
                }
                // Modifiers change matching inside the group: give up.
                _ => return None,
            }
        }
        let (atoms, nullable) = self.disjunction()?;
        if self.peek() != Some(c(')')) {
            return None;
        }
        self.i += 1;
        if lookaround {
            // Zero-width; Annex B lets a lookahead take a quantifier.
            return Some((Vec::new(), true, false));
        }
        Some((atoms, nullable, true))
    }

    /// `[...]` (not v-mode): to the closing bracket.
    fn class(&mut self) -> Option<()> {
        self.i += 1;
        if self.peek() == Some(c('^')) {
            self.i += 1;
        }
        loop {
            let u = self.peek()?;
            if u == c(']') {
                self.i += 1;
                return Some(());
            }
            if u == c('\\') {
                self.i += 1;
                self.peek()?;
            }
            self.i += 1;
        }
    }

    /// An escape outside a class that consumes one character; backreferences and Annex B oddities give up.
    fn escape(&mut self) -> Option<()> {
        self.i += 1;
        let e = self.peek()?;
        self.i += 1;
        match char::from_u32(e) {
            Some('0') => (!self.peek().is_some_and(is_digit)).then_some(()),
            Some('c') => {
                let letter = self.peek().and_then(char::from_u32).is_some_and(|x| x.is_ascii_alphabetic());
                if !letter {
                    return None;
                }
                self.i += 1;
                Some(())
            }
            Some('x') => {
                if self.peek().is_some_and(is_hex) && self.peek_at(1).is_some_and(is_hex) {
                    self.i += 2;
                }
                Some(())
            }
            Some('u') => {
                if self.peek() == Some(c('{')) && self.unicode {
                    while self.peek()? != c('}') {
                        self.i += 1;
                    }
                    self.i += 1;
                } else if (0..4).all(|k| self.peek_at(k).is_some_and(is_hex)) {
                    self.i += 4;
                    // A surrogate pair written as two escapes is one character in Unicode mode.
                    if self.unicode
                        && self.peek() == Some(c('\\'))
                        && self.peek_at(1) == Some(c('u'))
                        && (2..6).all(|k| self.peek_at(k).is_some_and(is_hex))
                    {
                        return None;
                    }
                } else if self.unicode {
                    return None;
                }
                Some(())
            }
            Some('p') | Some('P') if self.unicode => {
                if self.peek() != Some(c('{')) {
                    return None;
                }
                while self.peek()? != c('}') {
                    self.i += 1;
                }
                self.i += 1;
                Some(())
            }
            _ => Some(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms(p: &str) -> Option<Vec<String>> {
        let chars: Vec<u32> = p.chars().map(|c| c as u32).collect();
        first_atoms(&chars, false).map(|v| v.into_iter().map(|a| a.into_iter().map(|u| char::from_u32(u).unwrap()).collect()).collect())
    }

    #[test]
    fn collects_first_atoms() {
        assert_eq!(atoms("\\s+"), Some(vec!["\\s".into()]));
        assert_eq!(atoms("\\d+(?:\\.\\d+)?"), Some(vec!["\\d".into()]));
        assert_eq!(atoms("a?b|[cd]e"), Some(vec!["a".into(), "b".into(), "[cd]".into()]));
        assert_eq!(atoms("(?=x)y"), Some(vec!["y".into()]));
        assert_eq!(atoms("^\\bfoo"), Some(vec!["f".into()]));
        assert_eq!(atoms("a*"), None);
        assert_eq!(atoms("(a)\\1"), Some(vec!["a".into()]));
        assert_eq!(atoms("\\1a"), None);
        assert_eq!(atoms("x{0,2}y"), Some(vec!["x".into(), "y".into()]));
        assert_eq!(atoms("a{"), None);
        assert_eq!(atoms("(?i:a)"), None);
    }
}
