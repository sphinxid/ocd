//! Regular expression literal validation (acorn's `regexp.js`), for `ecmaVersion: "latest"`.
//!
//! This mirrors acorn's recursive-descent validator function by function, so it accepts exactly the patterns acorn
//! accepts and reports the same first error. Like acorn, it works on UTF-16 code units and only combines surrogate
//! pairs into code points in unicode (`u`/`v`) mode or where acorn forces it.
//!
//! Integer values (`lastIntValue`, quantifier bounds, back-reference numbers) are `f64` because acorn computes them
//! with JavaScript numbers, and long digit runs lose precision or overflow to `Infinity` the same way here.

use super::chars::{is_identifier_char, is_identifier_start};
use super::unicode_data::{BINARY_PROPERTIES, BINARY_PROPERTIES_OF_STRINGS, GENERAL_CATEGORY_VALUES, SCRIPT_VALUES};
use std::collections::HashMap;

type R<T> = Result<T, String>;

/// Validates the pattern of a regex literal whose flags were already validated (valid flag chars, no duplicates, not
/// both `u` and `v`).
///
/// Returns `Err(msg)` where `msg` is the text acorn appends after `"Invalid regular expression: /<source>/: "`.
pub(super) fn validate_regexp_pattern(source: &[u16], flags: &str) -> Result<(), String> {
    let unicode_sets = flags.contains('v');
    let unicode = flags.contains('u');
    let mut state = State {
        src: source,
        switch_u: unicode_sets || unicode,
        switch_v: unicode_sets,
        switch_n: unicode_sets || unicode,
        pos: 0,
        last_int_value: 0.0,
        last_string_value: String::new(),
        last_assertion_is_quantifiable: false,
        num_capturing_parens: 0,
        max_back_reference: 0.0,
        group_names: HashMap::new(),
        back_reference_names: Vec::new(),
        branch_id: None,
        branches: Vec::new(),
    };
    state.pattern()?;
    // The goal symbol is `Pattern[~U, ~N]`; if the pattern has a group name, reparse with `Pattern[~U, +N]`.
    if !state.switch_n && !state.group_names.is_empty() {
        state.switch_n = true;
        state.pattern()?;
    }
    Ok(())
}

/// What a character-set construct can match (acorn's `CharSetNone` / `CharSetOk` / `CharSetString`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharSet {
    /// Nothing parsed (acorn's falsy `0` / `null`).
    None,
    /// Parsed, cannot contain strings.
    Ok,
    /// Parsed, can contain strings.
    String,
}

/// A branch of a disjunction (acorn's `BranchID`), stored in an arena and compared by index.
#[derive(Debug, Clone, Copy)]
struct Branch {
    parent: Option<usize>,
    /// Identifies this set of sibling branches.
    base: usize,
}

/// acorn's `RegExpValidationState`.
struct State<'a> {
    src: &'a [u16],
    switch_u: bool,
    switch_v: bool,
    switch_n: bool,
    pos: usize,
    last_int_value: f64,
    last_string_value: String,
    last_assertion_is_quantifiable: bool,
    num_capturing_parens: u32,
    max_back_reference: f64,
    /// Group name to the branches it was declared in.
    group_names: HashMap<String, Vec<usize>>,
    back_reference_names: Vec<String>,
    branch_id: Option<usize>,
    branches: Vec<Branch>,
}

const BACKSLASH: i64 = 0x5c;

fn raise<T>(message: &str) -> R<T> {
    Err(message.to_owned())
}

fn is_syntax_character(ch: i64) -> bool {
    ch == 0x24
        || (0x28..=0x2b).contains(&ch)
        || ch == 0x2e
        || ch == 0x3f
        || (0x5b..=0x5e).contains(&ch)
        || (0x7b..=0x7d).contains(&ch)
}

fn is_regular_expression_modifier(ch: i64) -> bool {
    ch == 0x69 || ch == 0x6d || ch == 0x73
}

fn ident_code(ch: f64) -> Option<u32> {
    (ch >= 0.0 && ch <= u32::MAX as f64).then_some(ch as u32)
}

fn is_regexp_identifier_start(ch: f64) -> bool {
    ident_code(ch).is_some_and(|c| is_identifier_start(c, true)) || ch == 36.0 || ch == 95.0
}

fn is_regexp_identifier_part(ch: f64) -> bool {
    ident_code(ch).is_some_and(|c| is_identifier_char(c, true))
        || ch == 36.0
        || ch == 95.0
        || ch == 0x200c as f64
        || ch == 0x200d as f64
}

fn is_control_letter(ch: i64) -> bool {
    (0x41..=0x5a).contains(&ch) || (0x61..=0x7a).contains(&ch)
}

fn is_valid_unicode(ch: f64) -> bool {
    (0.0..=0x10ffff as f64).contains(&ch)
}

fn is_character_class_escape(ch: i64) -> bool {
    matches!(ch, 0x64 | 0x44 | 0x73 | 0x53 | 0x77 | 0x57)
}

fn is_unicode_property_name_character(ch: i64) -> bool {
    is_control_letter(ch) || ch == 0x5f
}

fn is_unicode_property_value_character(ch: i64) -> bool {
    is_unicode_property_name_character(ch) || is_decimal_digit(ch)
}

fn is_class_set_reserved_double_punctuator_character(ch: i64) -> bool {
    ch == 0x21
        || (0x23..=0x26).contains(&ch)
        || (0x2a..=0x2c).contains(&ch)
        || ch == 0x2e
        || (0x3a..=0x40).contains(&ch)
        || ch == 0x5e
        || ch == 0x60
        || ch == 0x7e
}

fn is_class_set_syntax_character(ch: i64) -> bool {
    ch == 0x28 || ch == 0x29 || ch == 0x2d || ch == 0x2f || (0x5b..=0x5d).contains(&ch) || (0x7b..=0x7d).contains(&ch)
}

fn is_class_set_reserved_punctuator(ch: i64) -> bool {
    matches!(ch, 0x21 | 0x23 | 0x25 | 0x26 | 0x2c | 0x2d | 0x3a..=0x3e | 0x40 | 0x60 | 0x7e)
}

fn is_decimal_digit(ch: i64) -> bool {
    (0x30..=0x39).contains(&ch)
}

fn is_hex_digit(ch: i64) -> bool {
    (0x30..=0x39).contains(&ch) || (0x41..=0x46).contains(&ch) || (0x61..=0x66).contains(&ch)
}

fn hex_to_int(ch: i64) -> f64 {
    (if (0x41..=0x46).contains(&ch) {
        10 + (ch - 0x41)
    } else if (0x61..=0x66).contains(&ch) {
        10 + (ch - 0x61)
    } else {
        ch - 0x30
    }) as f64
}

fn is_octal_digit(ch: i64) -> bool {
    (0x30..=0x37).contains(&ch)
}

fn in_sorted(list: &[&str], word: &str) -> bool {
    list.binary_search(&word).is_ok()
}

fn push_code_point(s: &mut String, ch: f64) {
    s.push(char::from_u32(ch as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
}

impl State<'_> {
    // ## Cursor helpers

    /// The code unit at `i`, or (in unicode mode or when `force_u`) the code point of a surrogate pair; `-1` at the
    /// end.
    fn at(&self, i: usize, force_u: bool) -> i64 {
        let l = self.src.len();
        if i >= l {
            return -1;
        }
        let c = self.src[i] as i64;
        if !(force_u || self.switch_u) || c <= 0xd7ff || c >= 0xe000 || i + 1 >= l {
            return c;
        }
        let next = self.src[i + 1] as i64;
        if (0xdc00..=0xdfff).contains(&next) { (c << 10) + next - 0x35fdc00 } else { c }
    }

    fn next_index(&self, i: usize, force_u: bool) -> usize {
        let l = self.src.len();
        if i >= l {
            return l;
        }
        let c = self.src[i];
        if !(force_u || self.switch_u) || c <= 0xd7ff || c >= 0xe000 || i + 1 >= l {
            return i + 1;
        }
        let next = self.src[i + 1];
        if !(0xdc00..=0xdfff).contains(&next) { i + 1 } else { i + 2 }
    }

    fn current(&self) -> i64 {
        self.at(self.pos, false)
    }

    fn current_u(&self, force_u: bool) -> i64 {
        self.at(self.pos, force_u)
    }

    fn lookahead(&self) -> i64 {
        self.at(self.next_index(self.pos, false), false)
    }

    fn advance(&mut self) {
        self.pos = self.next_index(self.pos, false);
    }

    fn advance_u(&mut self, force_u: bool) {
        self.pos = self.next_index(self.pos, force_u);
    }

    fn eat(&mut self, ch: i64) -> bool {
        if self.current() == ch {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_chars(&mut self, chs: &[i64]) -> bool {
        let mut pos = self.pos;
        for &ch in chs {
            let current = self.at(pos, false);
            if current == -1 || current != ch {
                return false;
            }
            pos = self.next_index(pos, false);
        }
        self.pos = pos;
        true
    }

    // ## Branch tracking

    fn new_branch(&mut self, parent: Option<usize>, base: Option<usize>) -> usize {
        let id = self.branches.len();
        self.branches.push(Branch { parent, base: base.unwrap_or(id) });
        id
    }

    /// acorn's `BranchID.prototype.separatedFrom`.
    fn separated_from(&self, this: usize, alt: usize) -> bool {
        let mut s = Some(this);
        while let Some(si) = s {
            let mut o = Some(alt);
            while let Some(oi) = o {
                if self.branches[si].base == self.branches[oi].base && si != oi {
                    return true;
                }
                o = self.branches[oi].parent;
            }
            s = self.branches[si].parent;
        }
        false
    }

    // ## Grammar

    fn pattern(&mut self) -> R<()> {
        self.pos = 0;
        self.last_int_value = 0.0;
        self.last_string_value.clear();
        self.last_assertion_is_quantifiable = false;
        self.num_capturing_parens = 0;
        self.max_back_reference = 0.0;
        self.group_names.clear();
        self.back_reference_names.clear();
        self.branch_id = None;

        self.disjunction()?;

        if self.pos != self.src.len() {
            // Make the same messages as V8.
            if self.eat(0x29) {
                return raise("Unmatched ')'");
            }
            if self.eat(0x5d) || self.eat(0x7d) {
                return raise("Lone quantifier brackets");
            }
        }
        if self.max_back_reference > self.num_capturing_parens as f64 {
            return raise("Invalid escape");
        }
        for name in &self.back_reference_names {
            if !self.group_names.contains_key(name) {
                return raise("Invalid named capture referenced");
            }
        }
        Ok(())
    }

    fn disjunction(&mut self) -> R<()> {
        let id = self.new_branch(self.branch_id, None);
        self.branch_id = Some(id);
        self.alternative()?;
        while self.eat(0x7c) {
            let cur = self.branches[self.branch_id.expect("inside a disjunction")];
            let sibling = self.new_branch(cur.parent, Some(cur.base));
            self.branch_id = Some(sibling);
            self.alternative()?;
        }
        self.branch_id = self.branches[self.branch_id.expect("inside a disjunction")].parent;

        // Make the same message as V8.
        if self.eat_quantifier(true)? {
            return raise("Nothing to repeat");
        }
        if self.eat(0x7b) {
            return raise("Lone quantifier brackets");
        }
        Ok(())
    }

    fn alternative(&mut self) -> R<()> {
        while self.pos < self.src.len() && self.eat_term()? {}
        Ok(())
    }

    fn eat_term(&mut self) -> R<bool> {
        if self.eat_assertion()? {
            // `QuantifiableAssertion Quantifier`.
            if self.last_assertion_is_quantifiable && self.eat_quantifier(false)? && self.switch_u {
                return raise("Invalid quantifier");
            }
            return Ok(true);
        }
        let atom = if self.switch_u { self.eat_atom()? } else { self.eat_extended_atom()? };
        if atom {
            self.eat_quantifier(false)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn eat_assertion(&mut self) -> R<bool> {
        let start = self.pos;
        self.last_assertion_is_quantifiable = false;

        // ^, $
        if self.eat(0x5e) || self.eat(0x24) {
            return Ok(true);
        }

        // \b \B
        if self.eat(BACKSLASH) {
            if self.eat(0x42) || self.eat(0x62) {
                return Ok(true);
            }
            self.pos = start;
        }

        // Lookahead / lookbehind.
        if self.eat(0x28) && self.eat(0x3f) {
            let lookbehind = self.eat(0x3c);
            if self.eat(0x3d) || self.eat(0x21) {
                self.disjunction()?;
                if !self.eat(0x29) {
                    return raise("Unterminated group");
                }
                self.last_assertion_is_quantifiable = !lookbehind;
                return Ok(true);
            }
        }

        self.pos = start;
        Ok(false)
    }

    fn eat_quantifier(&mut self, no_error: bool) -> R<bool> {
        if self.eat_quantifier_prefix(no_error)? {
            self.eat(0x3f);
            return Ok(true);
        }
        Ok(false)
    }

    fn eat_quantifier_prefix(&mut self, no_error: bool) -> R<bool> {
        Ok(self.eat(0x2a) || self.eat(0x2b) || self.eat(0x3f) || self.eat_braced_quantifier(no_error)?)
    }

    fn eat_braced_quantifier(&mut self, no_error: bool) -> R<bool> {
        let start = self.pos;
        if self.eat(0x7b) {
            let mut max = -1.0;
            if self.eat_decimal_digits() {
                let min = self.last_int_value;
                if self.eat(0x2c) && self.eat_decimal_digits() {
                    max = self.last_int_value;
                }
                if self.eat(0x7d) {
                    if max != -1.0 && max < min && !no_error {
                        return raise("numbers out of order in {} quantifier");
                    }
                    return Ok(true);
                }
            }
            if self.switch_u && !no_error {
                return raise("Incomplete quantifier");
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_atom(&mut self) -> R<bool> {
        Ok(self.eat_pattern_characters()
            || self.eat(0x2e)
            || self.eat_reverse_solidus_atom_escape()?
            || self.eat_character_class()?
            || self.eat_uncapturing_group()?
            || self.eat_capturing_group()?)
    }

    fn eat_reverse_solidus_atom_escape(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(BACKSLASH) {
            if self.eat_atom_escape()? {
                return Ok(true);
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_uncapturing_group(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(0x28) {
            if self.eat(0x3f) {
                let add = self.eat_modifiers();
                let has_hyphen = self.eat(0x2d);
                if !add.is_empty() || has_hyphen {
                    for (i, m) in add.char_indices() {
                        if add[i + 1..].contains(m) {
                            return raise("Duplicate regular expression modifiers");
                        }
                    }
                    if has_hyphen {
                        let remove = self.eat_modifiers();
                        if add.is_empty() && remove.is_empty() && self.current() == 0x3a {
                            return raise("Invalid regular expression modifiers");
                        }
                        for (i, m) in remove.char_indices() {
                            if remove[i + 1..].contains(m) || add.contains(m) {
                                return raise("Duplicate regular expression modifiers");
                            }
                        }
                    }
                }
                if self.eat(0x3a) {
                    self.disjunction()?;
                    if self.eat(0x29) {
                        return Ok(true);
                    }
                    return raise("Unterminated group");
                }
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_capturing_group(&mut self) -> R<bool> {
        if self.eat(0x28) {
            self.group_specifier()?;
            self.disjunction()?;
            if self.eat(0x29) {
                self.num_capturing_parens += 1;
                return Ok(true);
            }
            return raise("Unterminated group");
        }
        Ok(false)
    }

    fn eat_modifiers(&mut self) -> String {
        let mut modifiers = String::new();
        loop {
            let ch = self.current();
            if ch == -1 || !is_regular_expression_modifier(ch) {
                break;
            }
            modifiers.push(ch as u8 as char);
            self.advance();
        }
        modifiers
    }

    fn eat_extended_atom(&mut self) -> R<bool> {
        Ok(self.eat(0x2e)
            || self.eat_reverse_solidus_atom_escape()?
            || self.eat_character_class()?
            || self.eat_uncapturing_group()?
            || self.eat_capturing_group()?
            || self.eat_invalid_braced_quantifier()?
            || self.eat_extended_pattern_character())
    }

    fn eat_invalid_braced_quantifier(&mut self) -> R<bool> {
        if self.eat_braced_quantifier(true)? {
            return raise("Nothing to repeat");
        }
        Ok(false)
    }

    fn eat_syntax_character(&mut self) -> bool {
        let ch = self.current();
        if is_syntax_character(ch) {
            self.last_int_value = ch as f64;
            self.advance();
            return true;
        }
        false
    }

    fn eat_pattern_characters(&mut self) -> bool {
        let start = self.pos;
        loop {
            let ch = self.current();
            if ch == -1 || is_syntax_character(ch) {
                break;
            }
            self.advance();
        }
        self.pos != start
    }

    fn eat_extended_pattern_character(&mut self) -> bool {
        let ch = self.current();
        if ch != -1
            && ch != 0x24
            && !(0x28..=0x2b).contains(&ch)
            && ch != 0x2e
            && ch != 0x3f
            && ch != 0x5b
            && ch != 0x5e
            && ch != 0x7c
        {
            self.advance();
            return true;
        }
        false
    }

    fn group_specifier(&mut self) -> R<()> {
        if self.eat(0x3f) {
            if !self.eat_group_name()? {
                return raise("Invalid group");
            }
            let branch = self.branch_id.expect("inside a disjunction");
            if let Some(known) = self.group_names.get(&self.last_string_value) {
                for &alt in known {
                    if !self.separated_from(alt, branch) {
                        return raise("Duplicate capture group name");
                    }
                }
            }
            self.group_names.entry(self.last_string_value.clone()).or_default().push(branch);
        }
        Ok(())
    }

    fn eat_group_name(&mut self) -> R<bool> {
        self.last_string_value.clear();
        if self.eat(0x3c) {
            if self.eat_regexp_identifier_name()? && self.eat(0x3e) {
                return Ok(true);
            }
            return raise("Invalid capture group name");
        }
        Ok(false)
    }

    fn eat_regexp_identifier_name(&mut self) -> R<bool> {
        self.last_string_value.clear();
        if self.eat_regexp_identifier_start()? {
            push_code_point(&mut self.last_string_value, self.last_int_value);
            while self.eat_regexp_identifier_part()? {
                push_code_point(&mut self.last_string_value, self.last_int_value);
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn eat_regexp_identifier_start(&mut self) -> R<bool> {
        let start = self.pos;
        let mut ch = self.current_u(true) as f64;
        self.advance_u(true);
        if ch == BACKSLASH as f64 && self.eat_regexp_unicode_escape_sequence(true)? {
            ch = self.last_int_value;
        }
        if is_regexp_identifier_start(ch) {
            self.last_int_value = ch;
            return Ok(true);
        }
        self.pos = start;
        Ok(false)
    }

    fn eat_regexp_identifier_part(&mut self) -> R<bool> {
        let start = self.pos;
        let mut ch = self.current_u(true) as f64;
        self.advance_u(true);
        if ch == BACKSLASH as f64 && self.eat_regexp_unicode_escape_sequence(true)? {
            ch = self.last_int_value;
        }
        if is_regexp_identifier_part(ch) {
            self.last_int_value = ch;
            return Ok(true);
        }
        self.pos = start;
        Ok(false)
    }

    fn eat_atom_escape(&mut self) -> R<bool> {
        if self.eat_back_reference()
            || self.eat_character_class_escape()? != CharSet::None
            || self.eat_character_escape()?
            || (self.switch_n && self.eat_k_group_name()?)
        {
            return Ok(true);
        }
        if self.switch_u {
            // Make the same message as V8.
            if self.current() == 0x63 {
                return raise("Invalid unicode escape");
            }
            return raise("Invalid escape");
        }
        Ok(false)
    }

    fn eat_back_reference(&mut self) -> bool {
        let start = self.pos;
        if self.eat_decimal_escape() {
            let n = self.last_int_value;
            if self.switch_u {
                if n > self.max_back_reference {
                    self.max_back_reference = n;
                }
                return true;
            }
            if n <= self.num_capturing_parens as f64 {
                return true;
            }
            self.pos = start;
        }
        false
    }

    fn eat_k_group_name(&mut self) -> R<bool> {
        if self.eat(0x6b) {
            if self.eat_group_name()? {
                self.back_reference_names.push(self.last_string_value.clone());
                return Ok(true);
            }
            return raise("Invalid named reference");
        }
        Ok(false)
    }

    fn eat_character_escape(&mut self) -> R<bool> {
        Ok(self.eat_control_escape()
            || self.eat_c_control_letter()
            || self.eat_zero()
            || self.eat_hex_escape_sequence()?
            || self.eat_regexp_unicode_escape_sequence(false)?
            || (!self.switch_u && self.eat_legacy_octal_escape_sequence())
            || self.eat_identity_escape())
    }

    fn eat_c_control_letter(&mut self) -> bool {
        let start = self.pos;
        if self.eat(0x63) {
            if self.eat_control_letter() {
                return true;
            }
            self.pos = start;
        }
        false
    }

    fn eat_zero(&mut self) -> bool {
        if self.current() == 0x30 && !is_decimal_digit(self.lookahead()) {
            self.last_int_value = 0.0;
            self.advance();
            return true;
        }
        false
    }

    fn eat_control_escape(&mut self) -> bool {
        let value = match self.current() {
            0x74 => 0x09,
            0x6e => 0x0a,
            0x76 => 0x0b,
            0x66 => 0x0c,
            0x72 => 0x0d,
            _ => return false,
        };
        self.last_int_value = value as f64;
        self.advance();
        true
    }

    fn eat_control_letter(&mut self) -> bool {
        let ch = self.current();
        if is_control_letter(ch) {
            self.last_int_value = (ch % 0x20) as f64;
            self.advance();
            return true;
        }
        false
    }

    fn eat_regexp_unicode_escape_sequence(&mut self, force_u: bool) -> R<bool> {
        let start = self.pos;
        let switch_u = force_u || self.switch_u;
        if self.eat(0x75) {
            if self.eat_fixed_hex_digits(4) {
                let lead = self.last_int_value;
                if switch_u && (0xd800 as f64..=0xdbff as f64).contains(&lead) {
                    let lead_surrogate_end = self.pos;
                    if self.eat(BACKSLASH) && self.eat(0x75) && self.eat_fixed_hex_digits(4) {
                        let trail = self.last_int_value;
                        if (0xdc00 as f64..=0xdfff as f64).contains(&trail) {
                            self.last_int_value =
                                (lead - 0xd800 as f64) * 0x400 as f64 + (trail - 0xdc00 as f64) + 0x10000 as f64;
                            return Ok(true);
                        }
                    }
                    self.pos = lead_surrogate_end;
                    self.last_int_value = lead;
                }
                return Ok(true);
            }
            if switch_u
                && self.eat(0x7b)
                && self.eat_hex_digits()
                && self.eat(0x7d)
                && is_valid_unicode(self.last_int_value)
            {
                return Ok(true);
            }
            if switch_u {
                return raise("Invalid unicode escape");
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_identity_escape(&mut self) -> bool {
        if self.switch_u {
            if self.eat_syntax_character() {
                return true;
            }
            if self.eat(0x2f) {
                self.last_int_value = 0x2f as f64;
                return true;
            }
            return false;
        }
        let ch = self.current();
        if ch != 0x63 && (!self.switch_n || ch != 0x6b) {
            self.last_int_value = ch as f64;
            self.advance();
            return true;
        }
        false
    }

    fn eat_decimal_escape(&mut self) -> bool {
        self.last_int_value = 0.0;
        let mut ch = self.current();
        if (0x31..=0x39).contains(&ch) {
            loop {
                self.last_int_value = 10.0 * self.last_int_value + (ch - 0x30) as f64;
                self.advance();
                ch = self.current();
                if !is_decimal_digit(ch) {
                    break;
                }
            }
            return true;
        }
        false
    }

    fn eat_character_class_escape(&mut self) -> R<CharSet> {
        let ch = self.current();
        if is_character_class_escape(ch) {
            self.last_int_value = -1.0;
            self.advance();
            return Ok(CharSet::Ok);
        }
        if self.switch_u && (ch == 0x50 || ch == 0x70) {
            let negate = ch == 0x50;
            self.last_int_value = -1.0;
            self.advance();
            if self.eat(0x7b) {
                let result = self.eat_unicode_property_value_expression()?;
                if result != CharSet::None && self.eat(0x7d) {
                    if negate && result == CharSet::String {
                        return raise("Invalid property name");
                    }
                    return Ok(result);
                }
            }
            return raise("Invalid property name");
        }
        Ok(CharSet::None)
    }

    fn eat_unicode_property_value_expression(&mut self) -> R<CharSet> {
        let start = self.pos;

        // UnicodePropertyName `=` UnicodePropertyValue
        if self.eat_unicode_property_name() && self.eat(0x3d) {
            let name = std::mem::take(&mut self.last_string_value);
            if self.eat_unicode_property_value() {
                Self::validate_unicode_property_name_and_value(&name, &self.last_string_value)?;
                return Ok(CharSet::Ok);
            }
        }
        self.pos = start;

        // LoneUnicodePropertyNameOrValue
        if self.eat_unicode_property_value() {
            return self.validate_unicode_property_name_or_value(&self.last_string_value);
        }
        Ok(CharSet::None)
    }

    fn validate_unicode_property_name_and_value(name: &str, value: &str) -> R<()> {
        let values = match name {
            "General_Category" | "gc" => GENERAL_CATEGORY_VALUES,
            "Script" | "sc" | "Script_Extensions" | "scx" => SCRIPT_VALUES,
            _ => return raise("Invalid property name"),
        };
        if !in_sorted(values, value) {
            return raise("Invalid property value");
        }
        Ok(())
    }

    fn validate_unicode_property_name_or_value(&self, name_or_value: &str) -> R<CharSet> {
        if in_sorted(BINARY_PROPERTIES, name_or_value) {
            return Ok(CharSet::Ok);
        }
        if self.switch_v && in_sorted(BINARY_PROPERTIES_OF_STRINGS, name_or_value) {
            return Ok(CharSet::String);
        }
        raise("Invalid property name")
    }

    fn eat_unicode_property_name(&mut self) -> bool {
        self.last_string_value.clear();
        loop {
            let ch = self.current();
            if !is_unicode_property_name_character(ch) {
                break;
            }
            self.last_string_value.push(ch as u8 as char);
            self.advance();
        }
        !self.last_string_value.is_empty()
    }

    fn eat_unicode_property_value(&mut self) -> bool {
        self.last_string_value.clear();
        loop {
            let ch = self.current();
            if !is_unicode_property_value_character(ch) {
                break;
            }
            self.last_string_value.push(ch as u8 as char);
            self.advance();
        }
        !self.last_string_value.is_empty()
    }

    fn eat_character_class(&mut self) -> R<bool> {
        if self.eat(0x5b) {
            let negate = self.eat(0x5e);
            let result = self.class_contents()?;
            if !self.eat(0x5d) {
                return raise("Unterminated character class");
            }
            if negate && result == CharSet::String {
                return raise("Negated character class may contain strings");
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn class_contents(&mut self) -> R<CharSet> {
        if self.current() == 0x5d {
            return Ok(CharSet::Ok);
        }
        if self.switch_v {
            return self.class_set_expression();
        }
        self.non_empty_class_ranges()?;
        Ok(CharSet::Ok)
    }

    fn non_empty_class_ranges(&mut self) -> R<()> {
        while self.eat_class_atom()? {
            let left = self.last_int_value;
            if self.eat(0x2d) && self.eat_class_atom()? {
                let right = self.last_int_value;
                if self.switch_u && (left == -1.0 || right == -1.0) {
                    return raise("Invalid character class");
                }
                if left != -1.0 && right != -1.0 && left > right {
                    return raise("Range out of order in character class");
                }
            }
        }
        Ok(())
    }

    fn eat_class_atom(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(BACKSLASH) {
            if self.eat_class_escape()? {
                return Ok(true);
            }
            if self.switch_u {
                // Make the same message as V8.
                let ch = self.current();
                if ch == 0x63 || is_octal_digit(ch) {
                    return raise("Invalid class escape");
                }
                return raise("Invalid escape");
            }
            self.pos = start;
        }
        let ch = self.current();
        // acorn would loop forever on an unterminated class; the tokenizer guarantees a closing `]` is found first.
        if ch != 0x5d && ch != -1 {
            self.last_int_value = ch as f64;
            self.advance();
            return Ok(true);
        }
        Ok(false)
    }

    fn eat_class_escape(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(0x62) {
            self.last_int_value = 0x08 as f64;
            return Ok(true);
        }
        if self.switch_u && self.eat(0x2d) {
            self.last_int_value = 0x2d as f64;
            return Ok(true);
        }
        if !self.switch_u && self.eat(0x63) {
            if self.eat_class_control_letter() {
                return Ok(true);
            }
            self.pos = start;
        }
        Ok(self.eat_character_class_escape()? != CharSet::None || self.eat_character_escape()?)
    }

    fn class_set_expression(&mut self) -> R<CharSet> {
        let mut result = CharSet::Ok;
        if self.eat_class_set_range()? {
            // Fall through to the union.
        } else {
            let sub = self.eat_class_set_operand()?;
            if sub == CharSet::None {
                return raise("Invalid character in character class");
            }
            if sub == CharSet::String {
                result = CharSet::String;
            }
            // ClassIntersection
            let start = self.pos;
            while self.eat_chars(&[0x26, 0x26]) {
                if self.current() != 0x26 {
                    let sub = self.eat_class_set_operand()?;
                    if sub != CharSet::None {
                        if sub != CharSet::String {
                            result = CharSet::Ok;
                        }
                        continue;
                    }
                }
                return raise("Invalid character in character class");
            }
            if start != self.pos {
                return Ok(result);
            }
            // ClassSubtraction
            while self.eat_chars(&[0x2d, 0x2d]) {
                if self.eat_class_set_operand()? != CharSet::None {
                    continue;
                }
                return raise("Invalid character in character class");
            }
            if start != self.pos {
                return Ok(result);
            }
        }
        // ClassUnion
        loop {
            if self.eat_class_set_range()? {
                continue;
            }
            let sub = self.eat_class_set_operand()?;
            if sub == CharSet::None {
                return Ok(result);
            }
            if sub == CharSet::String {
                result = CharSet::String;
            }
        }
    }

    fn eat_class_set_range(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat_class_set_character()? {
            let left = self.last_int_value;
            if self.eat(0x2d) && self.eat_class_set_character()? {
                let right = self.last_int_value;
                if left != -1.0 && right != -1.0 && left > right {
                    return raise("Range out of order in character class");
                }
                return Ok(true);
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_class_set_operand(&mut self) -> R<CharSet> {
        if self.eat_class_set_character()? {
            return Ok(CharSet::Ok);
        }
        let result = self.eat_class_string_disjunction()?;
        if result != CharSet::None {
            return Ok(result);
        }
        self.eat_nested_class()
    }

    fn eat_nested_class(&mut self) -> R<CharSet> {
        let start = self.pos;
        if self.eat(0x5b) {
            let negate = self.eat(0x5e);
            let result = self.class_contents()?;
            if self.eat(0x5d) {
                if negate && result == CharSet::String {
                    return raise("Negated character class may contain strings");
                }
                return Ok(result);
            }
            self.pos = start;
        }
        if self.eat(BACKSLASH) {
            let result = self.eat_character_class_escape()?;
            if result != CharSet::None {
                return Ok(result);
            }
            self.pos = start;
        }
        Ok(CharSet::None)
    }

    fn eat_class_string_disjunction(&mut self) -> R<CharSet> {
        let start = self.pos;
        if self.eat_chars(&[BACKSLASH, 0x71]) {
            if self.eat(0x7b) {
                let result = self.class_string_disjunction_contents()?;
                if self.eat(0x7d) {
                    return Ok(result);
                }
            } else {
                // Make the same message as V8.
                return raise("Invalid escape");
            }
            self.pos = start;
        }
        Ok(CharSet::None)
    }

    fn class_string_disjunction_contents(&mut self) -> R<CharSet> {
        let mut result = self.class_string()?;
        while self.eat(0x7c) {
            if self.class_string()? == CharSet::String {
                result = CharSet::String;
            }
        }
        Ok(result)
    }

    fn class_string(&mut self) -> R<CharSet> {
        let mut count = 0;
        while self.eat_class_set_character()? {
            count += 1;
        }
        Ok(if count == 1 { CharSet::Ok } else { CharSet::String })
    }

    fn eat_class_set_character(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(BACKSLASH) {
            if self.eat_character_escape()? || self.eat_class_set_reserved_punctuator() {
                return Ok(true);
            }
            if self.eat(0x62) {
                self.last_int_value = 0x08 as f64;
                return Ok(true);
            }
            self.pos = start;
            return Ok(false);
        }
        let ch = self.current();
        if ch < 0 || (ch == self.lookahead() && is_class_set_reserved_double_punctuator_character(ch)) {
            return Ok(false);
        }
        if is_class_set_syntax_character(ch) {
            return Ok(false);
        }
        self.advance();
        self.last_int_value = ch as f64;
        Ok(true)
    }

    fn eat_class_set_reserved_punctuator(&mut self) -> bool {
        let ch = self.current();
        if is_class_set_reserved_punctuator(ch) {
            self.last_int_value = ch as f64;
            self.advance();
            return true;
        }
        false
    }

    fn eat_class_control_letter(&mut self) -> bool {
        let ch = self.current();
        if is_decimal_digit(ch) || ch == 0x5f {
            self.last_int_value = (ch % 0x20) as f64;
            self.advance();
            return true;
        }
        false
    }

    fn eat_hex_escape_sequence(&mut self) -> R<bool> {
        let start = self.pos;
        if self.eat(0x78) {
            if self.eat_fixed_hex_digits(2) {
                return Ok(true);
            }
            if self.switch_u {
                return raise("Invalid escape");
            }
            self.pos = start;
        }
        Ok(false)
    }

    fn eat_decimal_digits(&mut self) -> bool {
        let start = self.pos;
        self.last_int_value = 0.0;
        loop {
            let ch = self.current();
            if !is_decimal_digit(ch) {
                break;
            }
            self.last_int_value = 10.0 * self.last_int_value + (ch - 0x30) as f64;
            self.advance();
        }
        self.pos != start
    }

    fn eat_hex_digits(&mut self) -> bool {
        let start = self.pos;
        self.last_int_value = 0.0;
        loop {
            let ch = self.current();
            if !is_hex_digit(ch) {
                break;
            }
            self.last_int_value = 16.0 * self.last_int_value + hex_to_int(ch);
            self.advance();
        }
        self.pos != start
    }

    /// Legacy octal escapes `\0`-`\377`.
    fn eat_legacy_octal_escape_sequence(&mut self) -> bool {
        if self.eat_octal_digit() {
            let n1 = self.last_int_value;
            if self.eat_octal_digit() {
                let n2 = self.last_int_value;
                if n1 <= 3.0 && self.eat_octal_digit() {
                    self.last_int_value += n1 * 64.0 + n2 * 8.0;
                } else {
                    self.last_int_value = n1 * 8.0 + n2;
                }
            } else {
                self.last_int_value = n1;
            }
            return true;
        }
        false
    }

    fn eat_octal_digit(&mut self) -> bool {
        let ch = self.current();
        if is_octal_digit(ch) {
            self.last_int_value = (ch - 0x30) as f64;
            self.advance();
            return true;
        }
        self.last_int_value = 0.0;
        false
    }

    fn eat_fixed_hex_digits(&mut self, length: usize) -> bool {
        let start = self.pos;
        self.last_int_value = 0.0;
        for _ in 0..length {
            let ch = self.current();
            if !is_hex_digit(ch) {
                self.pos = start;
                return false;
            }
            self.last_int_value = 16.0 * self.last_int_value + hex_to_int(ch);
            self.advance();
        }
        true
    }
}
