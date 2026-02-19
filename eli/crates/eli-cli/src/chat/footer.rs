fn has_interactive_terminal() -> bool {
    if env_truthy("ELI_PLAIN_OUTPUT") || env_truthy("ELI_NO_FOOTER") {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.eq_ignore_ascii_case("dumb") {
            return false;
        }
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let v = value.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}

impl FooterUi {
    fn enable() -> Self {
        if !has_interactive_terminal() {
            let (w, h) = terminal_size();
            return Self {
                height: 3,
                active: false,
                term_width: w,
                term_height: h,
            };
        }

        terminal::enable_raw_mode().ok();
        let mut out = std::io::stdout();
        queue!(out, cursor::Hide).ok();
        out.flush().ok();
        let (w, h) = terminal_size();
        let mut this = Self {
            height: 3,
            active: true,
            term_width: w,
            term_height: h,
        };
        // Clear footer area before setting up scroll region
        this.clear_footer_rows(&mut out);
        this.apply_scroll_region();
        this
    }

    fn disable(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Clear footer area before resetting scroll region
        let mut out = std::io::stdout();
        self.clear_footer_rows(&mut out);
        self.reset_scroll_region();
        queue!(out, cursor::Show).ok();
        out.flush().ok();
        terminal::disable_raw_mode().ok();
    }

    fn clear_footer_rows(&self, out: &mut std::io::Stdout) {
        // Reset scroll region temporarily so we can write anywhere
        write!(out, "\x1b[r").ok();
        let footer_top = self.term_height.saturating_sub(self.height as usize);
        for row in footer_top..self.term_height {
            write!(out, "\x1b[{};1H\x1b[2K", row + 1).ok();
        }
        out.flush().ok();
    }

    fn apply_scroll_region(&mut self) {
        let bottom = self.term_height.saturating_sub(self.height as usize).max(1);
        let mut out = std::io::stdout();
        // DECSTBM: set scroll region to exclude footer rows.
        write!(out, "\x1b[1;{}r", bottom).ok();
        // Keep cursor in scrollable region.
        write!(out, "\x1b[{};1H", bottom).ok();
        out.flush().ok();
    }

    fn reset_scroll_region(&self) {
        let mut out = std::io::stdout();
        write!(out, "\x1b[r").ok();
        out.flush().ok();
    }

    fn render(&mut self, title: &str, input: &str, cursor_pos: usize) {
        if !self.active {
            return;
        }
        let (width, height) = terminal_size();
        if width != self.term_width || height != self.term_height {
            let mut out = std::io::stdout();

            // 1. Reset scroll region so we can clear anywhere
            write!(out, "\x1b[r").ok();

            // 2. Save cursor, clear from old footer to end of screen using ED command
            let old_footer_top = self.term_height.saturating_sub(self.height as usize);
            let new_footer_top = height.saturating_sub(self.height as usize);
            let clear_from = old_footer_top.min(new_footer_top);

            // Move to the earliest possible footer position and clear to end of screen
            write!(out, "\x1b[{};1H", clear_from + 1).ok(); // Move to row
            write!(out, "\x1b[J").ok(); // Clear from cursor to end of screen (ED0)
            out.flush().ok();

            // 3. Update dimensions and apply new scroll region
            self.term_width = width;
            self.term_height = height;
            self.apply_scroll_region();
        }
        let footer_top = height.saturating_sub(self.height as usize);
        let rect = Rect::new(0, 0, width as u16, self.height);
        let mut buf = Buffer::empty(rect);

        // TUI-style cursor rendering
        let inner_width = width.saturating_sub(4).max(1); // Account for borders and prompt
        let prompt = "› ";
        let cursor_pos = cursor_pos.min(input.len());
        let (before_cursor, after_cursor) = input.split_at(cursor_pos);

        // Get character at cursor (or space if at end)
        let cursor_char = after_cursor.chars().next().unwrap_or(' ');
        let rest = if after_cursor.len() > cursor_char.len_utf8() {
            &after_cursor[cursor_char.len_utf8()..]
        } else {
            ""
        };

        // Build styled line with block cursor
        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(before_cursor, Style::default().fg(Color::White)),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::styled(rest, Style::default().fg(Color::White)),
        ]);

        Clear.render(rect, &mut buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Cyan))
            .title_style(Style::new().fg(Color::Cyan))
            .title(title);
        let paragraph = Paragraph::new(line).block(block);
        paragraph.render(rect, &mut buf);

        let mut out = std::io::stdout();
        flush_buffer(&mut out, &buf, rect, footer_top as u16);
        let scroll_y = footer_top.saturating_sub(1);
        queue!(out, cursor::MoveTo(0, scroll_y as u16)).ok();
        out.flush().ok();
    }
}

impl Drop for FooterUi {
    fn drop(&mut self) {
        self.disable();
    }
}

