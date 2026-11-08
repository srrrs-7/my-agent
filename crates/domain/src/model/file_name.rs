//! File names as a case-insensitive filesystem compares them.
//!
//! The access policy and the dependency gate both decide by name, and both
//! are only as good as their idea of when two names are the same file. On a
//! case-insensitive filesystem that is not "equal up to ASCII case". APFS -
//! macOS, and a Docker bind mount of a macOS directory - compares names after
//! full Unicode case folding and canonical decomposition, so `x.\u{212A}ey`
//! (KELVIN SIGN) opens `x.key` and `package-lock.j\u{17F}on` (LONG S) opens
//! `package-lock.json`. Both were verified on APFS directly and through a
//! virtiofs bind mount; a comparison that only lowercases ASCII lets either
//! spelling reach the protected file.
//!
//! Folding a name completely needs the Unicode tables, which the domain does
//! not carry. For an ASCII name it does not have to: case folding and NFD
//! both work character by character (the only reordering NFD does moves
//! combining marks, none of which is ASCII), so a name can fold to an
//! all-ASCII name only if every one of its characters does. [`TO_ASCII`]
//! lists every non-ASCII character that does, which makes [`fold`] exact for
//! the names every built-in rule is written in. Each entry was checked to
//! alias its ASCII spelling on APFS. (APFS ignores no code point - zero-width
//! characters keep two names apart - so nothing needs to be dropped.)
//!
//! A rule written with non-ASCII characters gets per-character lowercasing,
//! which unifies case but not canonically equivalent spellings: `é` and
//! `e` + U+0301 still compare different here although APFS opens one as the
//! other. Such a rule must list both spellings.

/// Every non-ASCII character whose Unicode case fold, decomposed (NFD), is
/// entirely ASCII - from the Unicode 13 data - plus U+0131, which NTFS's
/// upper-case comparison takes for `i` although Unicode folding does not.
const TO_ASCII: [(char, &str); 14] = [
    ('\u{00DF}', "ss"),  // LATIN SMALL LETTER SHARP S
    ('\u{0131}', "i"),   // LATIN SMALL LETTER DOTLESS I (NTFS only)
    ('\u{017F}', "s"),   // LATIN SMALL LETTER LONG S
    ('\u{037E}', ";"),   // GREEK QUESTION MARK
    ('\u{1E9E}', "ss"),  // LATIN CAPITAL LETTER SHARP S
    ('\u{1FEF}', "`"),   // GREEK VARIA
    ('\u{212A}', "k"),   // KELVIN SIGN
    ('\u{FB00}', "ff"),  // LATIN SMALL LIGATURE FF
    ('\u{FB01}', "fi"),  // LATIN SMALL LIGATURE FI
    ('\u{FB02}', "fl"),  // LATIN SMALL LIGATURE FL
    ('\u{FB03}', "ffi"), // LATIN SMALL LIGATURE FFI
    ('\u{FB04}', "ffl"), // LATIN SMALL LIGATURE FFL
    ('\u{FB05}', "st"),  // LATIN SMALL LIGATURE LONG S T
    ('\u{FB06}', "st"),  // LATIN SMALL LIGATURE ST
];

/// `name` in the form names are compared in: two names that a
/// case-insensitive filesystem may take for the same file fold to the same
/// string whenever either of them is ASCII (see the module documentation).
///
/// The result is for comparison only. It can be longer than `name` and
/// should never be used to open anything.
pub(crate) fn fold(name: &str) -> String {
    let mut folded = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii() {
            folded.push(c.to_ascii_lowercase());
        } else if let Some((_, ascii)) = TO_ASCII.iter().find(|(from, _)| *from == c) {
            folded.push_str(ascii);
        } else {
            folded.extend(c.to_lowercase());
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_a_case_insensitive_filesystem_opens_as_an_ascii_name_fold_to_it() {
        for (spelling, ascii) in [
            ("X.KEY", "x.key"),
            ("x.\u{212A}ey", "x.key"),
            ("package-lock.j\u{17F}on", "package-lock.json"),
            ("PACKAGE-LOCK.J\u{17F}ON", "package-lock.json"),
            ("\u{131}d_rsa", "id_rsa"),
            ("\u{FB01}le", "file"),
            ("pa\u{DF}", "pass"),
            ("PA\u{1E9E}", "pass"),
            ("\u{FB06}uff", "stuff"),
        ] {
            assert_eq!(fold(spelling), ascii, "`{spelling}`");
        }
    }

    #[test]
    fn every_entry_folds_to_ascii() {
        for (from, ascii) in TO_ASCII {
            assert!(!from.is_ascii(), "U+{:04X} is ASCII already", from as u32);
            assert!(
                ascii.is_ascii() && !ascii.is_empty(),
                "U+{:04X}",
                from as u32
            );
            assert_eq!(fold(&from.to_string()), ascii);
        }
    }

    #[test]
    fn other_non_ascii_names_are_only_lowercased() {
        assert_eq!(fold("CAF\u{C9}.txt"), "caf\u{E9}.txt");
        assert_eq!(fold("\u{79D8}\u{5BC6}"), "\u{79D8}\u{5BC6}");
        assert_eq!(
            fold("x.\u{FF4B}ey"),
            "x.\u{FF4B}ey",
            "fullwidth letters are compatibility variants, which no filesystem folds"
        );
        assert_eq!(
            fold("a\u{200B}b"),
            "a\u{200B}b",
            "zero-width characters keep names apart"
        );
    }
}
