use std::{
    collections::HashMap,
    io::{stdout, Write},
    mem,
};

use crossterm::{
    cursor,
    event::{self, read, Event, KeyCode, KeyEvent, KeyModifiers},
    style::{self, Color, StyledContent, Stylize},
    terminal, ExecutableCommand, QueueableCommand,
};
use serde::{Deserialize, Serialize};
use tree_sitter::{Parser, Query, QueryCursor};
use tree_sitter_rust::HIGHLIGHT_QUERY;

use crate::{
    buffer::Buffer,
    config::{Config, KeyAction},
    log,
    theme::{Style, Theme},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Action {
    Undo,
    Quit,

    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,

    InsertCharAtCursorPos(char),
    DeletePreviousChar,
    NewLine,

    EnterMode(Mode),
    PageDown,
    PageUp,
    MoveToLineEnd,
    MoveToLineStart,
    DeleteCharAtCursorPos,
    DeleteCurrentLine,
    DeleteLineAt(usize),

    SetWaitingKeyAction(Box<KeyAction>),
    InsertLineAt(usize, Option<String>),
    MoveLineToViewportCenter,
    InsertLineBelowCursor,
    InsertLineAtCursor,
    MoveToBottom,
    MoveToTop,
    RemoveCharAt(u16, usize),
    UndoMultiple(Vec<Action>),
}

impl Action {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Mode {
    Normal,
    Insert,
}

#[derive(Debug)]
pub struct StyleInfo {
    start: usize,
    end: usize,
    style: Style,
}

impl StyleInfo {
    pub fn contains(&self, pos: usize) -> bool {
        pos >= self.start && pos < self.end
    }
}

pub struct Editor {
    config: Config,
    theme: Theme,
    buffer: Buffer,
    stdout: std::io::Stdout,
    size: (u16, u16),
    vtop: usize,
    vleft: u16,
    cx: u16,
    cy: u16,
    vx: u16,
    mode: Mode,
    waiting_key_action: Option<KeyAction>,
    undo_actions: Vec<Action>,
    insert_undo_actions: Vec<Action>,
}

impl Editor {
    pub fn new(config: Config, theme: Theme, buffer: Buffer) -> anyhow::Result<Self> {
        let mut stdout = stdout();
        terminal::enable_raw_mode()?;
        stdout
            .execute(terminal::EnterAlternateScreen)?
            .execute(terminal::Clear(terminal::ClearType::All))?;

        let vx = buffer.len().to_string().len() as u16 + 2 as u16;

        Ok(Editor {
            config,
            theme,
            buffer,
            stdout,
            vtop: 0,
            vleft: 0,
            cx: 0,
            cy: 0,
            vx,
            mode: Mode::Normal,
            size: terminal::size()?,
            waiting_key_action: None,
            undo_actions: vec![],
            insert_undo_actions: vec![],
        })
    }

    fn vwidth(&self) -> u16 {
        self.size.0
    }

    fn vheight(&self) -> u16 {
        self.size.1 - 2
    }

    fn line_length(&self) -> u16 {
        if let Some(line) = self.viewport_line(self.cy) {
            return line.len() as u16;
        }
        0
    }

    fn buffer_line(&self) -> usize {
        self.vtop + self.cy as usize
    }

    fn viewport_line(&self, n: u16) -> Option<String> {
        let buffer_line = self.vtop + n as usize;
        self.buffer.get(buffer_line)
    }

    fn set_cursor_style(&mut self) -> anyhow::Result<()> {
        self.stdout.queue(match self.waiting_key_action {
            Some(_) => cursor::SetCursorStyle::SteadyUnderScore,
            _ => match self.mode {
                Mode::Normal => cursor::SetCursorStyle::DefaultUserShape,
                Mode::Insert => cursor::SetCursorStyle::SteadyBar,
            },
        })?;

        Ok(())
    }

    fn gutter_width(&self) -> usize {
        self.buffer.len().to_string().len() + 1
    }

    fn draw_gutter(&mut self) -> anyhow::Result<()> {
        let width = self.gutter_width();
        let fg = self
            .theme
            .gutter_style
            .fg
            .unwrap_or(self.theme.style.fg.expect("fg is defined for theme"));
        let bg = self
            .theme
            .gutter_style
            .bg
            .unwrap_or(self.theme.style.bg.expect("bg is defined for theme"));

        for n in 0..self.vheight() as usize {
            let line_number = n + 1 + self.vtop as usize;
            if line_number > self.buffer.len() {
                continue;
            }
            self.stdout
                .queue(cursor::MoveTo(0, n as u16))?
                .queue(style::PrintStyledContent(
                    format!("{line_number:>width$} ", width = width,)
                        .with(fg)
                        .on(bg),
                ))?;
        }

        Ok(())
    }

    pub fn draw(&mut self) -> anyhow::Result<()> {
        self.stdout.queue(cursor::Hide)?;
        self.draw_gutter()?;
        self.draw_viewport()?;
        self.draw_statusline()?;
        self.stdout
            .queue(cursor::MoveTo(self.vx + self.cx, self.cy))?;
        self.set_cursor_style()?;
        self.stdout.queue(cursor::Show)?;
        self.stdout.flush()?;

        Ok(())
    }

    pub fn highlight(&self, code: &str) -> anyhow::Result<Vec<StyleInfo>> {
        let mut parser = Parser::new();
        let language = tree_sitter_rust::language();
        parser.set_language(language)?;

        let tree = parser.parse(&code, None).expect("parse works");
        let query = Query::new(language, HIGHLIGHT_QUERY)?;

        let mut colors = Vec::new();
        let mut cursor = QueryCursor::new();
        let matches = cursor.matches(&query, tree.root_node(), code.as_bytes());

        for mat in matches {
            for cap in mat.captures {
                let node = cap.node;
                let start = node.start_byte();
                let end = node.end_byte();
                let scope = query.capture_names()[cap.index as usize].as_str();
                let style = self.theme.get_style(scope);

                if let Some(style) = style {
                    colors.push(StyleInfo { start, end, style });
                }
            }
        }

        Ok(colors)
    }

    fn print_char(&mut self, x: u16, y: u16, c: char, style: &Style) -> anyhow::Result<()> {
        let style = style.to_content_style(&self.theme.style);
        let styled_content = StyledContent::new(style, c);

        self.stdout
            .queue(cursor::MoveTo(x, y))?
            .queue(style::PrintStyledContent(styled_content))?;

        Ok(())
    }

    fn fill_line(&mut self, x: u16, y: u16, style: &Style) -> anyhow::Result<()> {
        let width = self.vwidth().saturating_sub(x) as usize;
        let line_fill = " ".repeat(width);
        let style = style.to_content_style(&self.theme.style);
        let styled_content = StyledContent::new(style, line_fill);
        self.stdout
            .queue(cursor::MoveTo(x, y))?
            .queue(style::PrintStyledContent(styled_content))?;

        Ok(())
    }

    pub fn draw_viewport(&mut self) -> anyhow::Result<()> {
        let vbuffer = self.buffer.viewport(self.vtop, self.vheight() as usize);
        let style_info = self.highlight(&vbuffer)?;
        let vheight = self.vheight();
        let default_style = self.theme.style.clone();

        let mut x = self.vx;
        let mut y = 0;
        let mut iter = vbuffer.chars().enumerate().peekable();

        while let Some((pos, c)) = iter.next() {
            if c == '\n' || iter.peek().is_none() {
                if c != '\n' {
                    self.print_char(x, y, c, &default_style)?;
                    x += 1;
                }
                self.fill_line(x, y, &default_style)?;
                x = self.vx;
                y += 1;
                if y > vheight {
                    break;
                }
                continue;
            }

            if x < self.vwidth() {
                if let Some(style) = determine_style_for_position(&style_info, pos) {
                    self.print_char(x, y, c, &style)?;
                } else {
                    self.print_char(x, y, c, &default_style)?;
                }
            }
            x += 1;
        }

        while y < vheight {
            self.fill_line(self.vx, y, &default_style)?;
            y += 1;
        }

        Ok(())
    }

    pub fn draw_statusline(&mut self) -> anyhow::Result<()> {
        let mode = format!(" {:?} ", self.mode).to_uppercase();
        let file = format!(" {}", self.buffer.file.as_deref().unwrap_or("No Name"));
        let pos = format!(" {}:{} ", self.cx + 1, self.cy + 1);

        let file_width = self.size.0 - mode.len() as u16 - pos.len() as u16 - 2;

        self.stdout.queue(cursor::MoveTo(0, self.size.1 - 2))?;
        self.stdout.queue(style::PrintStyledContent(
            mode.with(Color::Rgb { r: 0, g: 0, b: 0 })
                .bold()
                .on(Color::Rgb {
                    r: 184,
                    g: 144,
                    b: 243,
                }),
        ))?;
        self.stdout.queue(style::PrintStyledContent(
            ""
                .with(Color::Rgb {
                    r: 184,
                    g: 144,
                    b: 243,
                })
                .on(Color::Rgb {
                    r: 67,
                    g: 70,
                    b: 89,
                }),
        ))?;
        self.stdout.queue(style::PrintStyledContent(
            format!("{:<width$}", file, width = file_width as usize)
                .with(Color::Rgb {
                    r: 255,
                    g: 255,
                    b: 255,
                })
                .bold()
                .on(Color::Rgb {
                    r: 67,
                    g: 70,
                    b: 89,
                }),
        ))?;
        self.stdout.queue(style::PrintStyledContent(
            ""
                .with(Color::Rgb {
                    r: 184,
                    g: 144,
                    b: 243,
                })
                .on(Color::Rgb {
                    r: 67,
                    g: 70,
                    b: 89,
                }),
        ))?;
        self.stdout.queue(style::PrintStyledContent(
            pos.with(Color::Rgb { r: 0, g: 0, b: 0 })
                .bold()
                .on(Color::Rgb {
                    r: 184,
                    g: 144,
                    b: 243,
                }),
        ))?;

        Ok(())
    }

    fn is_insert(&self) -> bool {
        matches!(self.mode, Mode::Insert)
    }

    // TODO: in neovim, when you are at an x position and you move to a shorter line, the cursor
    //       goes back to the max x but returns to the previous x position if the line is longer
    fn check_bounds(&mut self) {
        let line_length = self.line_length();

        if self.cx >= line_length && !self.is_insert() {
            if line_length > 0 {
                self.cx = self.line_length() - 1;
            } else if !self.is_insert() {
                self.cx = 0;
            }
        }
        if self.cx >= self.vwidth() {
            self.cx = self.vwidth() - 1;
        }

        // check if cy is after the end of the buffer
        // the end of the buffer is less than vtop + cy
        let line_on_buffer = self.cy as usize + self.vtop;
        if line_on_buffer > self.buffer.len() - 1 {
            self.cy = (self.buffer.len() as usize - self.vtop - 1) as u16;
        }
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.check_bounds();
            self.draw()?;
            if let Some(action) = self.handle_event(read()?) {
                log!("Action: {action:?}");
                let quit = match action {
                    KeyAction::Single(action) => self.execute(&action),
                    KeyAction::Multiple(actions) => {
                        let mut quit = false;
                        for action in actions {
                            if self.execute(&action) {
                                quit = true;
                                break;
                            }
                        }
                        quit
                    }
                    KeyAction::Nested(actions) => {
                        self.waiting_key_action = Some(KeyAction::Nested(actions));
                        false
                    }
                };

                if quit {
                    log!("requested to quit");
                    break;
                }
            }
        }

        Ok(())
    }

    fn handle_event(&mut self, ev: event::Event) -> Option<KeyAction> {
        if let event::Event::Resize(width, height) = ev {
            self.size = (width, height);
            return None;
        }

        if let Some(ka) = self.waiting_key_action.take() {
            return self.handle_waiting_command(ka, ev);
        }

        match self.mode {
            Mode::Normal => self.handle_normal_event(ev),
            Mode::Insert => self.handle_insert_event(ev),
        }
    }

    fn handle_waiting_command(&mut self, ka: KeyAction, ev: event::Event) -> Option<KeyAction> {
        let KeyAction::Nested(nested_mappings) = ka else {
            panic!("expected nested mappings");
        };

        event_to_key_action(&nested_mappings, &ev)
    }

    fn handle_insert_event(&self, ev: event::Event) -> Option<KeyAction> {
        if let Some(ka) = event_to_key_action(&self.config.keys.insert, &ev) {
            return Some(ka);
        }

        match ev {
            Event::Key(event) => match event.code {
                KeyCode::Char(c) => KeyAction::Single(Action::InsertCharAtCursorPos(c)).into(),
                _ => None,
            },
            _ => None,
        }
    }

    fn handle_normal_event(&mut self, ev: event::Event) -> Option<KeyAction> {
        event_to_key_action(&self.config.keys.normal, &ev)
    }

    // TODO: I don't think this handlers are ever gonna fail, so maybe just return Option<Action>
    // here?
    fn _handle_waiting_command(
        &self,
        cmd: char,
        ev: event::Event,
    ) -> anyhow::Result<Option<Action>> {
        let action = match cmd {
            'd' => match ev {
                event::Event::Key(event) => match event.code {
                    event::KeyCode::Char('d') => Some(Action::DeleteCurrentLine),
                    _ => None,
                },
                _ => None,
            },
            'g' => match ev {
                event::Event::Key(event) => match event.code {
                    event::KeyCode::Char('g') => Some(Action::MoveToTop),
                    _ => None,
                },
                _ => None,
            },
            'z' => match ev {
                event::Event::Key(event) => match event.code {
                    event::KeyCode::Char('z') => Some(Action::MoveLineToViewportCenter),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };

        Ok(action)
    }

    pub fn cleanup(&mut self) -> anyhow::Result<()> {
        self.stdout.execute(terminal::LeaveAlternateScreen)?;
        terminal::disable_raw_mode()?;

        Ok(())
    }

    fn current_line_contents(&self) -> Option<String> {
        self.buffer.get(self.buffer_line())
    }

    fn execute(&mut self, action: &Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::MoveUp => {
                if self.cy == 0 {
                    // scroll up
                    if self.vtop > 0 {
                        self.vtop -= 1;
                    }
                } else {
                    self.cy = self.cy.saturating_sub(1);
                }
            }
            Action::MoveDown => {
                self.cy += 1;
                if self.cy >= self.vheight() {
                    // scroll if possible
                    self.vtop += 1;
                    self.cy -= 1;
                }
            }
            Action::MoveLeft => {
                self.cx = self.cx.saturating_sub(1);
                if self.cx < self.vleft {
                    self.cx = self.vleft;
                }
            }
            Action::MoveRight => {
                self.cx += 1;
            }
            Action::MoveToLineStart => {
                self.cx = 0;
            }
            Action::MoveToLineEnd => {
                self.cx = self.line_length().saturating_sub(1);
            }
            Action::PageUp => {
                if self.vtop > 0 {
                    self.vtop = self.vtop.saturating_sub(self.vheight() as usize);
                }
            }
            Action::PageDown => {
                if self.buffer.len() > self.vtop + self.vheight() as usize {
                    self.vtop += self.vheight() as usize;
                }
            }
            Action::EnterMode(new_mode) => {
                // entering insert mode
                if !self.is_insert() && matches!(new_mode, Mode::Insert) {
                    self.insert_undo_actions = Vec::new();
                }
                if self.is_insert() && matches!(new_mode, Mode::Normal) {
                    if !self.insert_undo_actions.is_empty() {
                        let actions = mem::take(&mut self.insert_undo_actions);
                        self.undo_actions.push(Action::UndoMultiple(actions));
                    }
                }

                self.mode = *new_mode;
            }
            Action::InsertCharAtCursorPos(c) => {
                self.insert_undo_actions
                    .push(Action::RemoveCharAt(self.cx, self.buffer_line()));
                self.buffer.insert(self.cx, self.buffer_line(), *c);
                self.cx += 1;
            }
            Action::RemoveCharAt(x, y) => {
                self.buffer.remove(*x, *y);
            }
            Action::DeleteCharAtCursorPos => {
                self.buffer.remove(self.cx, self.buffer_line());
            }
            Action::NewLine => {
                self.cx = 0;
                self.cy += 1;
            }
            Action::SetWaitingKeyAction(key_action) => {
                self.waiting_key_action = Some(*(key_action.clone()));
            }
            Action::DeleteCurrentLine => {
                let line = self.buffer_line();
                let contents = self.current_line_contents();

                self.buffer.remove_line(self.buffer_line());
                self.undo_actions.push(Action::InsertLineAt(line, contents));
            }
            Action::Undo => {
                if let Some(undo_action) = self.undo_actions.pop() {
                    self.execute(&undo_action);
                }
            }
            Action::UndoMultiple(actions) => {
                for action in actions.iter().rev() {
                    self.execute(action);
                }
            }
            Action::InsertLineAt(y, contents) => {
                if let Some(contents) = contents {
                    self.buffer.insert_line(*y, contents.to_string());
                }
            }
            Action::MoveLineToViewportCenter => {
                let viewport_center = self.vheight() / 2;
                let distance_to_center = self.cy as isize - viewport_center as isize;

                if distance_to_center > 0 {
                    // if distance > 0 we need to scroll up
                    let distance_to_center = distance_to_center.abs() as usize;
                    if self.vtop > distance_to_center {
                        let new_vtop = self.vtop + distance_to_center;
                        self.vtop = new_vtop;
                        self.cy = viewport_center;
                    }
                } else if distance_to_center < 0 {
                    // if distance < 0 we need to scroll down
                    let distance_to_center = distance_to_center.abs() as usize;
                    let new_vtop = self.vtop.saturating_sub(distance_to_center);
                    let distance_to_go = self.vtop as usize + distance_to_center;
                    if self.buffer.len() > distance_to_go && new_vtop != self.vtop {
                        self.vtop = new_vtop;
                        self.cy = viewport_center;
                    }
                }
            }
            Action::InsertLineBelowCursor => {
                self.undo_actions
                    .push(Action::DeleteLineAt(self.buffer_line() + 1));

                self.buffer
                    .insert_line(self.buffer_line() + 1, String::new());
                self.cy += 1;
                self.cx = 0;
            }
            Action::InsertLineAtCursor => {
                self.undo_actions
                    .push(Action::DeleteLineAt(self.buffer_line()));

                self.buffer.insert_line(self.buffer_line(), String::new());
                self.cx = 0;
            }
            Action::MoveToTop => {
                self.vtop = 0;
                self.cy = 0;
            }
            Action::MoveToBottom => {
                if self.buffer.len() > self.vheight() as usize {
                    self.cy = self.vheight() - 1;
                    self.vtop = self.buffer.len() - self.vheight() as usize;
                } else {
                    self.cy = self.buffer.len() as u16 - 1u16;
                }
            }
            Action::DeleteLineAt(y) => self.buffer.remove_line(*y),
            Action::DeletePreviousChar => {
                if self.cx > 0 {
                    self.cx -= 1;
                    self.buffer.remove(self.cx, self.buffer_line());
                }
            }
        }

        false
    }
}

fn event_to_key_action(mappings: &HashMap<String, KeyAction>, ev: &Event) -> Option<KeyAction> {
    match ev {
        event::Event::Key(KeyEvent {
            code, modifiers, ..
        }) => {
            let key = match code {
                KeyCode::Char(c) => format!("{c}"),
                _ => format!("{code:?}"),
            };

            let key = match *modifiers {
                KeyModifiers::CONTROL => format!("Ctrl-{key}"),
                KeyModifiers::ALT => format!("Alt-{key}"),
                _ => key,
            };

            mappings.get(&key).cloned()
        }
        _ => None,
    }
}

fn determine_style_for_position(style_info: &Vec<StyleInfo>, pos: usize) -> Option<Style> {
    if let Some(s) = style_info.iter().find(|si| si.contains(pos)) {
        return Some(s.style.clone());
    }

    None
}
