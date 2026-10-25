//! Escape untrusted display values only at the terminal boundary.
use std::fmt;

pub(super) struct Terminal<T>(pub(super) T);

impl<T: fmt::Display> fmt::Display for Terminal<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Escaped<'a, 'b>(&'a mut fmt::Formatter<'b>);
        impl fmt::Write for Escaped<'_, '_> {
            fn write_str(&mut self, value: &str) -> fmt::Result {
                let mut start = 0;
                for (index, character) in value.char_indices() {
                    if !character.is_control() {
                        continue;
                    }
                    self.0.write_str(&value[start..index])?;
                    match character {
                        '\n' => self.0.write_str("\\n")?,
                        '\r' => self.0.write_str("\\r")?,
                        '\t' => self.0.write_str("\\t")?,
                        '\0' => self.0.write_str("\\0")?,
                        '\u{1}'..='\u{7f}' => write!(self.0, "\\x{:02x}", character as u32)?,
                        _ => write!(self.0, "\\u{{{:04x}}}", character as u32)?,
                    }
                    start = index + character.len_utf8();
                }
                self.0.write_str(&value[start..])
            }
        }
        use fmt::Write as _;
        let alternate = formatter.alternate();
        let mut output = Escaped(formatter);
        if alternate {
            write!(output, "{:#}", self.0)
        } else {
            write!(output, "{}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_format_preserves_and_escapes_every_error_cause() {
        let error = anyhow::anyhow!("cause\u{1b}[2J\n\u{009b}red").context("context\r\t\0\u{7f}");
        assert_eq!(
            format!("{:#}", Terminal(&error)),
            "context\\r\\t\\0\\x7f: cause\\x1b[2J\\n\\u{009b}red"
        );
        assert_eq!(format!("{}", Terminal("ordinary 中文")), "ordinary 中文");
    }
}
