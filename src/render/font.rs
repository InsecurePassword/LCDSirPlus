//! Compact deterministic 3x5 bitmap font, ported glyph-for-glyph from the
//! LCDForge Go renderer (`internal/render/font.go`). Unsupported runes render
//! as '?'.

type Glyph = [u8; 5];

fn font(r: char) -> Option<Glyph> {
    Some(match r {
        ' ' => [0, 0, 0, 0, 0],
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '4' => [5, 5, 7, 1, 1],
        '5' => [7, 4, 7, 1, 7],
        '6' => [7, 4, 7, 5, 7],
        '7' => [7, 1, 1, 1, 1],
        '8' => [7, 5, 7, 5, 7],
        '9' => [7, 5, 7, 1, 7],
        'A' => [2, 5, 7, 5, 5],
        'B' => [6, 5, 6, 5, 6],
        'C' => [3, 4, 4, 4, 3],
        'D' => [6, 5, 5, 5, 6],
        'E' => [7, 4, 6, 4, 7],
        'F' => [7, 4, 6, 4, 4],
        'G' => [3, 4, 5, 5, 3],
        'H' => [5, 5, 7, 5, 5],
        'I' => [7, 2, 2, 2, 7],
        'J' => [1, 1, 1, 5, 2],
        'K' => [5, 5, 6, 5, 5],
        'L' => [4, 4, 4, 4, 7],
        'M' => [5, 7, 7, 5, 5],
        'N' => [5, 7, 7, 7, 5],
        'O' => [2, 5, 5, 5, 2],
        'P' => [6, 5, 6, 4, 4],
        'Q' => [2, 5, 5, 3, 1],
        'R' => [6, 5, 6, 5, 5],
        'S' => [3, 4, 2, 1, 6],
        'T' => [7, 2, 2, 2, 2],
        'U' => [5, 5, 5, 5, 7],
        'V' => [5, 5, 5, 5, 2],
        'W' => [5, 5, 7, 7, 5],
        'X' => [5, 5, 2, 5, 5],
        'Y' => [5, 5, 2, 2, 2],
        'Z' => [7, 1, 2, 4, 7],
        '-' => [0, 0, 7, 0, 0],
        '_' => [0, 0, 0, 0, 7],
        '.' => [0, 0, 0, 0, 2],
        ',' => [0, 0, 0, 2, 4],
        ':' => [0, 2, 0, 2, 0],
        ';' => [0, 2, 0, 2, 4],
        '/' => [1, 1, 2, 4, 4],
        '\\' => [4, 4, 2, 1, 1],
        '%' => [5, 1, 2, 4, 5],
        '+' => [0, 2, 7, 2, 0],
        '=' => [0, 7, 0, 7, 0],
        '[' => [6, 4, 4, 4, 6],
        ']' => [3, 1, 1, 1, 3],
        '(' => [2, 4, 4, 4, 2],
        ')' => [2, 1, 1, 1, 2],
        '<' => [1, 2, 4, 2, 1],
        '>' => [4, 2, 1, 2, 4],
        '!' => [2, 2, 2, 0, 2],
        '?' => [6, 1, 2, 0, 2],
        '#' => [5, 7, 5, 7, 5],
        '*' => [0, 5, 2, 5, 0],
        '|' => [2, 2, 2, 2, 2],
        '\u{00B0}' => [2, 5, 2, 0, 0], // °
        '@' => [2, 5, 7, 4, 3],
        '&' => [2, 5, 2, 5, 3],
        '"' => [5, 5, 0, 0, 0],
        '\'' => [2, 2, 0, 0, 0],
        _ => return None,
    })
}

fn normalize_rune(r: char) -> char {
    if r.is_ascii_lowercase() {
        return r.to_ascii_uppercase();
    }
    match r {
        '\u{2013}' | '\u{2014}' => '-', // en dash, em dash
        '\u{2026}' => '.',              // horizontal ellipsis
        '\u{2103}' => 'C',              // ℃
        '\u{2109}' => 'F',              // ℉
        other => {
            if font(other).is_some() {
                other
            } else {
                '?'
            }
        }
    }
}

pub fn text_width(text: &str, scale: i32) -> i32 {
    let scale = scale.max(1);
    let n = text.chars().count() as i32;
    if n == 0 {
        return 0;
    }
    n * (3 * scale + scale) - scale
}

impl super::Frame {
    pub fn text(&mut self, x: i32, y: i32, text: &str, on: bool) {
        self.text_scaled(x, y, text, 1, on);
    }

    pub fn text_scaled(&mut self, x: i32, y: i32, text: &str, scale: i32, on: bool) {
        let scale = scale.max(1);
        let mut cx = x;
        for r in text.chars() {
            if let Some(g) = font(normalize_rune(r)) {
                for (row, &bits) in g.iter().enumerate() {
                    for col in 0..3 {
                        if bits & (1 << (2 - col)) != 0 {
                            self.fill_rect(
                                cx + col * scale,
                                y + row as i32 * scale,
                                scale,
                                scale,
                                on,
                            );
                        }
                    }
                }
            }
            cx += 4 * scale;
        }
    }

    pub fn text_right(&mut self, right: i32, y: i32, text: &str, on: bool) {
        self.text(right - text_width(text, 1) + 1, y, text, on);
    }

    pub fn text_centered(
        &mut self,
        left: i32,
        width: i32,
        y: i32,
        text: &str,
        scale: i32,
        on: bool,
    ) {
        let w = text_width(text, scale);
        self.text_scaled(left + (width - w) / 2, y, text, scale, on);
    }
}

/// Truncate with an ellipsis until the text fits `max_width` at `scale`.
pub fn fit_text(text: &str, max_width: i32, scale: i32) -> String {
    if text_width(text, scale) <= max_width {
        return text.to_string();
    }
    const ELLIPSIS: &str = "...";
    let mut r: Vec<char> = text.chars().collect();
    while !r.is_empty() {
        r.pop();
        let candidate: String = r.iter().collect::<String>() + ELLIPSIS;
        if text_width(&candidate, scale) <= max_width {
            return candidate;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_matches_go_formula() {
        assert_eq!(text_width("", 1), 0);
        assert_eq!(text_width("MEM", 1), 11);
        assert_eq!(text_width("VMEM", 1), 15);
        // n*(3*scale+scale) - scale = 2*8 - 2 = 14.
        assert_eq!(text_width("AB", 2), 14);
    }

    #[test]
    fn lowercase_normalizes_to_uppercase() {
        let mut f = super::super::Frame::new();
        f.text(0, 0, "a", true);
        let mut g = super::super::Frame::new();
        g.text(0, 0, "A", true);
        assert!(f.equal(&g));
    }

    #[test]
    fn fit_text_appends_ellipsis() {
        assert_eq!(fit_text("HELLO", 100, 1), "HELLO");
        let fitted = fit_text("HELLO WORLD", 20, 1);
        assert!(fitted.ends_with("..."));
        assert!(text_width(&fitted, 1) <= 20);
        assert_eq!(fit_text("HELLO", 1, 1), "");
    }
}
