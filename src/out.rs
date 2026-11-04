//! Operator-facing output, laid out the one way ADR-0050 decides: a symbol and
//! a word on every item line, colour that only reinforces them, fixed columns,
//! and wrapping only on a terminal.
//!
//! A [`Doc`] renders to a `String` for an explicit [`Style`], so every
//! command's output is tested in plain mode, and [`Style::stdout`] /
//! [`Style::stderr`] decide that style for a real stream:
//!
//! - **Colour** per stream, by `anstream`'s rules (`NO_COLOR`, `CLICOLOR`,
//!   `CLICOLOR_FORCE`, `TERM`, `CI`), overridden by the global `--color`
//!   ([`set_color`]). Only the eight basic ANSI foreground colours and bold,
//!   so the operator's theme picks the shades; dim only for hints that repeat
//!   something said elsewhere.
//! - **Width** on a terminal only: `ioctl(TIOCGWINSZ)` on that stream, else
//!   `COLUMNS`, else 80, clamped to 60..=100. Not on a terminal nothing wraps,
//!   so one item is one line and `grep` finds it whole.
//!
//! The content never depends on the stream: a plan piped to a file says what
//! the one on the screen says.
//!
//! **Sanitize, then colour.** Every string a [`Doc`] is given goes through
//! [`crate::term::sanitize`] here, whoever wrote it, and the fixed SGR codes
//! are added after. So the only escape sequences in what faber prints are its
//! own, and callers need no sanitizing sink of their own. Line breaks in given
//! text are dropped too: the renderer is the only place that breaks a line.

use std::borrow::Cow;

use anstyle::{AnsiColor, Style as Sgr};

/// Where an item's kind (`+ create`) starts.
pub const KIND_COL: usize = 2;
/// Where an item's subject starts: the kind is 9 wide, then two spaces.
pub const SUBJECT_COL: usize = 13;
/// Where an item's detail starts, on its line or the next: the subject is 24
/// wide, then at least two spaces.
pub const DETAIL_COL: usize = 39;
/// Where a section header's counts start.
pub const COUNTS_COL: usize = 46;
/// Where an at-a-glance line's kind starts, under its heading.
pub const GLANCE_COL: usize = 4;
/// The kind column's width: `x refused`.
const KIND_WIDTH: usize = 9;
/// What a command broken between arguments continues by, past its `$`.
const CMD_INDENT: usize = 4;
/// The narrowest a detail column may be before the detail moves to its own
/// line, under the subject.
const MIN_DETAIL: usize = 30;
const MIN_WIDTH: usize = 60;
const MAX_WIDTH: usize = 100;
const FALLBACK_WIDTH: usize = 80;
/// The gap between count groups, and between a label and what follows it.
const SEP: &str = "   ";

/// What an item is: what happens to it, or what it is found to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Create,
    Change,
    /// Uninstall's kind.
    Remove,
    /// Nothing runs until a person fixes this.
    Refused,
    /// Needed and not done, so a capability stays off; the run goes on.
    Warning,
    Ok,
}

impl Kind {
    /// Every kind, in the order counts print in.
    pub const ALL: [Kind; 6] = [
        Kind::Create,
        Kind::Change,
        Kind::Remove,
        Kind::Warning,
        Kind::Refused,
        Kind::Ok,
    ];

    /// The ASCII symbol that leads the kind. `ok` has none.
    pub fn symbol(self) -> Option<char> {
        match self {
            Kind::Create => Some('+'),
            Kind::Change => Some('~'),
            Kind::Remove => Some('-'),
            Kind::Refused => Some('x'),
            Kind::Warning => Some('!'),
            Kind::Ok => None,
        }
    }

    /// The word after the symbol: what a screen reader reads, and what a
    /// reader without colour goes by.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Create => "create",
            Kind::Change => "change",
            Kind::Remove => "remove",
            Kind::Refused => "refused",
            Kind::Warning => "warning",
            Kind::Ok => "ok",
        }
    }

    /// `+ create`, or `ok`.
    pub fn label(self) -> String {
        match self.symbol() {
            Some(c) => format!("{c} {}", self.word()),
            None => self.word().to_string(),
        }
    }

    fn sgr(self) -> Sgr {
        let fg = |c: AnsiColor| Sgr::new().fg_color(Some(c.into()));
        match self {
            Kind::Create => fg(AnsiColor::Green),
            Kind::Change => fg(AnsiColor::Cyan),
            Kind::Remove => fg(AnsiColor::Red),
            Kind::Refused => fg(AnsiColor::Red).bold(),
            Kind::Warning => fg(AnsiColor::Yellow),
            Kind::Ok => Sgr::new(),
        }
    }

    /// The word in a count: `to create` in a summary, `create` in a section
    /// header; `warning` / `warnings` by number, and `refused` and `ok` as they
    /// are.
    fn count_word(self, n: usize, summary: bool) -> &'static str {
        match (self, summary) {
            (Kind::Create, true) => "to create",
            (Kind::Change, true) => "to change",
            (Kind::Remove, true) => "to remove",
            (Kind::Warning, _) if n == 1 => "warning",
            (Kind::Warning, _) => "warnings",
            (k, _) => k.word(),
        }
    }
}

/// How a [`Doc`] renders: with colour or not, wrapped at a width or not, and
/// verbose or not. The content depends on `verbose` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    /// The terminal's width, already clamped; `None` wraps nothing.
    pub width: Option<usize>,
    pub verbose: bool,
}

impl Style {
    /// No colour and no wrapping: what a pipe or a log gets.
    pub const PLAIN: Style = Style {
        color: false,
        width: None,
        verbose: false,
    };

    /// The style for standard output, as this process finds it.
    pub fn stdout(verbose: bool) -> Style {
        let stream = std::io::stdout();
        let color = anstream::AutoStream::choice(&stream) != anstream::ColorChoice::Never;
        Style {
            color,
            width: width_for(&stream, 1),
            verbose,
        }
    }

    /// The style for standard error, decided on its own.
    pub fn stderr(verbose: bool) -> Style {
        let stream = std::io::stderr();
        let color = anstream::AutoStream::choice(&stream) != anstream::ColorChoice::Never;
        Style {
            color,
            width: width_for(&stream, 2),
            verbose,
        }
    }

    /// This style, verbose.
    pub fn verbose(self, verbose: bool) -> Style {
        Style { verbose, ..self }
    }
}

/// `--color`: a global flag on every command, because `sudo`'s `env_reset`
/// drops the operator's `NO_COLOR` on its way to `sudo faber setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorWhen {
    /// By the stream and the environment (`NO_COLOR`, `CLICOLOR`, `TERM`).
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorWhen {
    /// As it is written on a command line.
    pub fn as_arg(self) -> &'static str {
        match self {
            ColorWhen::Auto => "auto",
            ColorWhen::Always => "always",
            ColorWhen::Never => "never",
        }
    }
}

/// Make `when` this process's colour choice, over the environment's.
pub fn set_color(when: ColorWhen) {
    let choice = match when {
        ColorWhen::Auto => anstream::ColorChoice::Auto,
        ColorWhen::Always => anstream::ColorChoice::Always,
        ColorWhen::Never => anstream::ColorChoice::Never,
    };
    choice.write_global();
}

/// The width to wrap a stream at: `None` unless it is a terminal.
fn width_for(stream: &impl std::io::IsTerminal, fd: i32) -> Option<usize> {
    if !stream.is_terminal() {
        return None;
    }
    Some(clamp_width(
        terminal_columns(fd),
        std::env::var("COLUMNS").ok().as_deref(),
    ))
}

/// The terminal's columns, else `COLUMNS`, else 80, clamped to 60..=100.
fn clamp_width(ioctl: Option<usize>, columns: Option<&str>) -> usize {
    ioctl
        .filter(|&n| n > 0)
        .or_else(|| {
            columns
                .and_then(|c| c.trim().parse().ok())
                .filter(|&n| n > 0)
        })
        .unwrap_or(FALLBACK_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod winsize {
    use std::os::raw::{c_int, c_ulong, c_ushort};

    /// `<sys/ttycom.h>` on macOS, `<asm-generic/ioctls.h>` on Linux.
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: c_ulong = 0x4008_7468;
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: c_ulong = 0x5413;

    /// The kernel writes all four fields; only `ws_col` is read.
    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct Winsize {
        ws_row: c_ushort,
        ws_col: c_ushort,
        ws_xpixel: c_ushort,
        ws_ypixel: c_ushort,
    }

    extern "C" {
        /// POSIX. Variadic in C, and declared variadic here for the same
        /// reason as `openat` in `setup::fsx`: Apple's arm64 ABI passes
        /// variadic arguments on the stack, so a fixed-argument declaration
        /// would hand the kernel garbage for the pointer.
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    /// The columns of the terminal on `fd`, if it is one and says.
    pub(super) fn columns(fd: c_int) -> Option<usize> {
        let mut ws = Winsize::default();
        // SAFETY: TIOCGWINSZ writes one `struct winsize` through the pointer,
        // which is valid and exclusively borrowed for the call.
        let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize) };
        (rc == 0 && ws.ws_col > 0).then_some(usize::from(ws.ws_col))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn terminal_columns(fd: i32) -> Option<usize> {
    winsize::columns(fd)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn terminal_columns(_fd: i32) -> Option<usize> {
    None
}

/// How many items of each kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub create: usize,
    pub change: usize,
    pub remove: usize,
    pub warning: usize,
    pub refused: usize,
    pub ok: usize,
}

impl Counts {
    pub fn of(kinds: impl IntoIterator<Item = Kind>) -> Counts {
        let mut c = Counts::default();
        for k in kinds {
            c.add(k);
        }
        c
    }

    pub fn add(&mut self, kind: Kind) {
        *self.slot(kind) += 1;
    }

    pub fn get(&self, kind: Kind) -> usize {
        match kind {
            Kind::Create => self.create,
            Kind::Change => self.change,
            Kind::Remove => self.remove,
            Kind::Warning => self.warning,
            Kind::Refused => self.refused,
            Kind::Ok => self.ok,
        }
    }

    fn slot(&mut self, kind: Kind) -> &mut usize {
        match kind {
            Kind::Create => &mut self.create,
            Kind::Change => &mut self.change,
            Kind::Remove => &mut self.remove,
            Kind::Warning => &mut self.warning,
            Kind::Refused => &mut self.refused,
            Kind::Ok => &mut self.ok,
        }
    }

    /// Whether anything is to be created, changed or removed.
    pub fn any_change(&self) -> bool {
        self.create + self.change + self.remove > 0
    }
}

/// One paragraph of an item's detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Para {
    /// A sentence or more, single-spaced; the renderer wraps it.
    Text(String),
    /// A command quoted in the detail (a refusal's fix), one argument per
    /// element, each already quoted for a shell. Printed `$ ` first, broken
    /// only between arguments.
    Command(Vec<String>),
}

/// A command `--verbose` lists under its item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// One argument per element, each already quoted for a shell.
    pub args: Vec<String>,
    /// A clause about when or how it runs, printed under it.
    pub note: Option<String>,
}

impl Command {
    pub fn of(args: Vec<String>) -> Command {
        Command { args, note: None }
    }

    pub fn note(mut self, note: impl Into<String>) -> Command {
        self.note = Some(note.into());
        self
    }
}

/// One item line, and what `--verbose` adds under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// What it is on disk: a path, an account by name, a service's file.
    pub subject: String,
    pub detail: Vec<Para>,
    /// The detail `--verbose` prints instead, when it says more.
    pub verbose_detail: Option<Vec<Para>>,
    /// The commands `--verbose` lists.
    pub commands: Vec<Command>,
}

impl Item {
    pub fn new(kind: Kind, subject: impl Into<String>) -> Item {
        Item {
            kind,
            subject: subject.into(),
            detail: Vec::new(),
            verbose_detail: None,
            commands: Vec::new(),
        }
    }

    /// Add a paragraph of text to the detail. Empty text adds nothing.
    pub fn detail(mut self, text: impl Into<String>) -> Item {
        let text = text.into();
        if !text.trim().is_empty() {
            self.detail.push(Para::Text(text));
        }
        self
    }

    /// Add a quoted command to the detail.
    pub fn detail_command(mut self, args: Vec<String>) -> Item {
        self.detail.push(Para::Command(args));
        self
    }

    /// The detail `--verbose` prints instead of the default one.
    pub fn verbose_detail(mut self, text: impl Into<String>) -> Item {
        let text = text.into();
        if !text.trim().is_empty() {
            self.verbose_detail
                .get_or_insert_with(Vec::new)
                .push(Para::Text(text));
        }
        self
    }

    pub fn command(mut self, command: Command) -> Item {
        self.commands.push(command);
        self
    }
}

/// A document being rendered for one [`Style`].
#[derive(Debug)]
pub struct Doc {
    style: Style,
    out: String,
}

impl Doc {
    pub fn new(style: Style) -> Doc {
        Doc {
            style,
            out: String::new(),
        }
    }

    pub fn style(&self) -> Style {
        self.style
    }

    pub fn finish(self) -> String {
        self.out
    }

    pub fn blank(&mut self) {
        self.out.push('\n');
    }

    /// The document's title, bold, at column 0.
    pub fn title(&mut self, text: &str) {
        let text = one_line(text);
        self.paint(Sgr::new().bold(), &text);
        self.out.push('\n');
    }

    /// A heading inside the summary ("At a glance"), bold, at column 2.
    pub fn heading(&mut self, text: &str) {
        self.pad(KIND_COL);
        let text = one_line(text);
        self.paint(Sgr::new().bold(), &text);
        self.out.push('\n');
    }

    /// Prose at `indent`, wrapped under itself.
    pub fn text(&mut self, indent: usize, text: &str) {
        self.pad(indent);
        let words = words(text);
        self.wrap(&words, indent, indent);
        self.out.push('\n');
    }

    /// A text the reader has seen elsewhere, dimmed: a hint.
    pub fn hint(&mut self, indent: usize, text: &str) {
        self.pad(indent);
        let text = one_line(text);
        self.paint(Sgr::new().dimmed(), &text);
        self.out.push('\n');
    }

    /// A header fact: `  label     value`, the value at column 12 and wrapped
    /// under itself.
    pub fn fact(&mut self, label: &str, value: &str) {
        let label = one_line(label);
        self.pad(KIND_COL);
        self.out.push_str(&format!("{label:<8}  "));
        let col = KIND_COL + width(&label).max(8) + 2;
        let words = words(value);
        self.wrap(&words, col, col);
        self.out.push('\n');
    }

    /// The counts as a verdict, `+ 29 to create   x 0 refused`: create,
    /// change, remove and warnings when there are any, refused always, `ok`
    /// never. `hint` is dimmed in parentheses after them.
    pub fn summary(&mut self, indent: usize, counts: &Counts, hint: Option<&str>) {
        self.pad(indent);
        let mut first = true;
        for kind in Kind::ALL {
            let n = counts.get(kind);
            if kind == Kind::Ok || (n == 0 && kind != Kind::Refused) {
                continue;
            }
            if !first {
                self.out.push_str(SEP);
            }
            first = false;
            self.count(kind, n, true);
        }
        if let Some(hint) = hint {
            self.out.push_str(SEP);
            self.paint(Sgr::new().dimmed(), &format!("({})", one_line(hint)));
        }
        self.out.push('\n');
    }

    /// The summary's verdict followed by a sentence, `x 1 refused: nothing was
    /// changed`, as one line at column 0.
    pub fn verdict(&mut self, kind: Kind, n: usize, rest: &str) {
        self.count(kind, n, true);
        self.out.push_str(&one_line(rest));
        self.out.push('\n');
    }

    /// A section header: its title in bold, and its counts at
    /// [`COUNTS_COL`], every kind it has, `ok` included.
    pub fn section(&mut self, title: &str, counts: &Counts) {
        let title = one_line(title);
        self.paint(Sgr::new().bold(), &title);
        let mut first = true;
        for kind in Kind::ALL {
            let n = counts.get(kind);
            if n == 0 {
                continue;
            }
            if first {
                let at = width(&title);
                self.pad_to(at, COUNTS_COL.max(at + 2));
            } else {
                self.out.push_str(SEP);
            }
            first = false;
            self.count(kind, n, false);
        }
        self.out.push('\n');
    }

    /// An at-a-glance line: a kind, and one sentence, under its heading.
    pub fn glance(&mut self, kind: Kind, text: &str) {
        self.pad(GLANCE_COL);
        self.kind(kind);
        self.out.push_str("  ");
        let col = GLANCE_COL + KIND_WIDTH + 2;
        let words = words(text);
        self.wrap(&words, col, col);
        self.out.push('\n');
    }

    /// One item: its kind, its subject, its detail at [`DETAIL_COL`] (on the
    /// subject's line when the subject leaves a two-space gap, else on the
    /// next), and in `--verbose` its commands under the subject.
    pub fn item(&mut self, item: &Item) {
        let detail_col = self.detail_col();
        self.pad(KIND_COL);
        self.kind(item.kind);
        self.out.push_str("  ");
        let subject = one_line(&item.subject);
        self.out.push_str(&subject);
        let mut at = SUBJECT_COL + width(&subject);
        let detail = match &item.verbose_detail {
            Some(v) if self.style.verbose => v,
            _ => &item.detail,
        };
        for (i, para) in detail.iter().enumerate() {
            if i == 0 && detail_col > SUBJECT_COL && at + 2 <= detail_col {
                self.pad_to(at, detail_col);
            } else {
                self.out.push('\n');
                self.pad(detail_col);
            }
            at = match para {
                Para::Text(text) => self.wrap(&words(text), detail_col, detail_col),
                Para::Command(args) => self.command(args, detail_col),
            };
        }
        self.out.push('\n');
        if self.style.verbose {
            for cmd in &item.commands {
                self.pad(SUBJECT_COL);
                self.command(&cmd.args, SUBJECT_COL);
                self.out.push('\n');
                if let Some(note) = &cmd.note {
                    // In parentheses, so a copied block shows it is no command.
                    self.pad(SUBJECT_COL + 2);
                    let note = format!("({note})");
                    self.wrap(&words(&note), SUBJECT_COL + 2, SUBJECT_COL + 2);
                    self.out.push('\n');
                }
            }
        }
    }

    /// The detail column: [`DETAIL_COL`], or the subject's when the terminal
    /// leaves the detail fewer than 30 columns there.
    fn detail_col(&self) -> usize {
        match self.style.width {
            Some(w) if w < DETAIL_COL + MIN_DETAIL => SUBJECT_COL,
            _ => DETAIL_COL,
        }
    }

    /// `$ ` and a command's arguments from the current column `col`, broken
    /// only between arguments with a trailing ` \` and continued
    /// [`CMD_INDENT`] further in. Returns the column it ends at.
    fn command(&mut self, args: &[String], col: usize) -> usize {
        self.out.push_str("$ ");
        let args: Vec<String> = args.iter().map(|a| one_line(a).into_owned()).collect();
        let mut at = col + 2;
        let cont = col + CMD_INDENT;
        for (i, arg) in args.iter().enumerate() {
            let w = width(arg);
            if i > 0 {
                // Room for the argument, and for ` \` if more follow it.
                let tail = if i + 1 < args.len() { 2 } else { 0 };
                let fits = self.style.width.is_none_or(|max| at + 1 + w + tail <= max);
                if fits {
                    self.out.push(' ');
                    at += 1;
                } else {
                    self.out.push_str(" \\\n");
                    self.pad(cont);
                    at = cont;
                }
            }
            self.out.push_str(arg);
            at += w;
        }
        at
    }

    /// Words from the current column `first`, wrapped at the style's width
    /// under `hang`. A word that does not fit on a fresh line overflows.
    /// Returns the column it ends at.
    fn wrap(&mut self, words: &[String], first: usize, hang: usize) -> usize {
        let mut at = first;
        let mut line_start = true;
        for word in words {
            let w = width(word);
            if !line_start {
                let fits = self.style.width.is_none_or(|max| at + 1 + w <= max);
                if fits {
                    self.out.push(' ');
                    at += 1;
                } else {
                    self.out.push('\n');
                    self.pad(hang);
                    at = hang;
                }
            }
            self.out.push_str(word);
            at += w;
            line_start = false;
        }
        at
    }

    /// A kind's symbol and word, coloured, padded to the kind column's width.
    fn kind(&mut self, kind: Kind) {
        let label = kind.label();
        self.paint(kind.sgr(), &label);
        self.pad_to(width(&label), KIND_WIDTH);
    }

    /// `+ 29 to create`, coloured as its kind.
    fn count(&mut self, kind: Kind, n: usize, summary: bool) {
        let text = match kind.symbol() {
            Some(c) => format!("{c} {n} {}", kind.count_word(n, summary)),
            None => format!("{} {n}", kind.count_word(n, summary)),
        };
        self.paint(kind.sgr(), &text);
    }

    fn paint(&mut self, sgr: Sgr, text: &str) {
        if self.style.color && sgr != Sgr::new() {
            self.out
                .push_str(&format!("{}{text}{}", sgr.render(), sgr.render_reset()));
        } else {
            self.out.push_str(text);
        }
    }

    fn pad(&mut self, n: usize) {
        self.out.extend(std::iter::repeat_n(' ', n));
    }

    fn pad_to(&mut self, at: usize, col: usize) {
        self.pad(col.saturating_sub(at));
    }
}

/// `text` made safe to print ([`crate::term::sanitize`]) and kept to one
/// line: a newline or tab in it is shown as `\n` or `\t`, never acted on.
fn one_line(text: &str) -> Cow<'_, str> {
    let clean = crate::term::sanitize(text);
    if !clean.contains(['\n', '\t']) {
        return clean;
    }
    Cow::Owned(clean.replace('\n', "\\n").replace('\t', "\\t"))
}

/// `text` made safe to print, as words: any run of whitespace, a newline
/// included, is one break point. Source text carries no layout.
/// A path with a space in it stays one word, so wrapping never splits it.
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in crate::term::sanitize(text).split_whitespace() {
        match out.last_mut() {
            Some(last) if keeps_together(last, word) => {
                last.push(' ');
                last.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

/// Whether the space between two words is inside a path, which is never
/// split: `/Library/Application Support/faber` has a `/` on both sides, and
/// `…/Application Support` ends a path with a capitalised segment. A
/// heuristic: a path whose last segment has a space and starts in lower case
/// can still be split at that space.
fn keeps_together(before: &str, after: &str) -> bool {
    before.contains('/')
        && !after.starts_with('/')
        && (after.contains('/')
            || (after.starts_with(|c: char| c.is_ascii_uppercase())
                && !before.ends_with(['.', ',', ';', ':', ')'])))
}

/// Columns, counted in `char`s: a wide character may misalign by one, which
/// ADR-0050 accepts.
fn width(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: Style = Style {
        color: false,
        width: Some(100),
        verbose: false,
    };

    fn render(style: Style, f: impl FnOnce(&mut Doc)) -> String {
        let mut doc = Doc::new(style);
        f(&mut doc);
        doc.finish()
    }

    fn col_of(line: &str, needle: &str) -> usize {
        line.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"))
    }

    #[test]
    fn every_kind_has_a_symbol_and_a_word_in_ascii() {
        for kind in Kind::ALL {
            assert!(kind.label().is_ascii());
            assert!(kind.label().ends_with(kind.word()));
            assert!(kind.label().chars().count() <= KIND_WIDTH);
        }
        assert_eq!(Kind::Refused.label(), "x refused");
        assert_eq!(Kind::Ok.label(), "ok");
    }

    #[test]
    fn item_lines_sit_at_the_fixed_columns() {
        let out = render(WIDE, |d| {
            d.item(&Item::new(Kind::Create, "group _faber").detail("gid 450"));
            d.item(&Item::new(Kind::Ok, "config check").detail("checked"));
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "  + create   group _faber              gid 450");
        assert_eq!(lines[1], "  ok         config check              checked");
        assert_eq!(col_of(lines[0], "+"), KIND_COL);
        assert_eq!(col_of(lines[0], "group"), SUBJECT_COL);
        assert_eq!(col_of(lines[0], "gid"), DETAIL_COL);
    }

    #[test]
    fn a_long_subject_puts_its_detail_on_the_next_line_at_the_detail_column() {
        // 24 characters leaves no two-space gap before column 39.
        let subject = "a".repeat(25);
        for style in [WIDE, Style::PLAIN] {
            let out = render(style, |d| {
                d.item(&Item::new(Kind::Change, &subject).detail("why"));
                d.item(&Item::new(Kind::Change, "a".repeat(24)).detail("fits"));
            });
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(lines[0], format!("  ~ change   {subject}"));
            assert_eq!(col_of(lines[1], "why"), DETAIL_COL);
            assert_eq!(col_of(lines[2], "fits"), DETAIL_COL, "{out}");
        }
    }

    #[test]
    fn wrapping_hangs_under_the_detail_column_and_never_splits_a_path() {
        let detail = "copied from /Users/owner/Library/Application Support/faber/config.toml \
                      and the original stays where it is, as the backup the operator keeps";
        let out = render(
            Style {
                width: Some(60),
                ..WIDE
            },
            |d| d.item(&Item::new(Kind::Create, "x").detail(detail)),
        );
        // 60 columns leave 21 at the detail column: the detail moves under
        // the subject.
        for line in out.lines().skip(1) {
            assert_eq!(line.len() - line.trim_start().len(), SUBJECT_COL, "{out}");
        }
        assert!(out.contains("/Users/owner/Library/Application Support/faber/config.toml"));

        let out = render(WIDE, |d| {
            d.item(&Item::new(Kind::Create, "x").detail(detail))
        });
        for (i, line) in out.lines().enumerate() {
            assert!(line.chars().count() <= 100, "{out}");
            if i > 0 {
                assert_eq!(line.len() - line.trim_start().len(), DETAIL_COL, "{out}");
            }
        }
        assert!(out.lines().count() > 1);
    }

    #[test]
    fn a_glance_line_sits_under_its_heading() {
        let out = render(WIDE, |d| {
            d.heading("At a glance");
            d.glance(Kind::Create, "two role accounts");
            d.glance(Kind::Warning, "VM jobs cannot run yet");
        });
        assert_eq!(
            out,
            "  At a glance\n    + create   two role accounts\n    ! warning  VM jobs cannot run yet\n"
        );
    }

    #[test]
    fn plain_output_never_wraps() {
        let long = "word ".repeat(60);
        let out = render(Style::PLAIN, |d| {
            d.item(&Item::new(Kind::Warning, "/var/db/faber/bin/faber-vm").detail(&long));
            d.glance(Kind::Warning, &long);
        });
        assert_eq!(out.lines().count(), 3, "{out}");
    }

    #[test]
    fn verbose_commands_sit_under_the_subject_and_break_between_arguments() {
        let args: Vec<String> = [
            "/usr/sbin/sysadminctl",
            "-addUser",
            "_faber",
            "-UID",
            "450",
            "-GID",
            "450",
            "-shell",
            "/usr/bin/false",
            "-home",
            "\"/Library/Application Support/faber\"",
            "-roleAccount",
        ]
        .map(String::from)
        .to_vec();
        let item = Item::new(Kind::Create, "user _faber")
            .detail("uid 450, hidden, no password")
            .command(Command::of(args.clone()))
            .command(
                Command::of(vec!["/usr/bin/dscl".into(), "-delete".into()])
                    .note("only if it has one"),
            );
        let quiet = render(WIDE, |d| d.item(&item));
        assert!(!quiet.contains('$'), "{quiet}");

        let out = render(WIDE.verbose(true), |d| d.item(&item));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[1],
            "             $ /usr/sbin/sysadminctl -addUser _faber -UID 450 -GID 450 -shell /usr/bin/false -home \\"
        );
        assert_eq!(
            lines[2],
            "                 \"/Library/Application Support/faber\" -roleAccount"
        );
        assert_eq!(lines[3], "             $ /usr/bin/dscl -delete");
        assert_eq!(lines[4], "               (only if it has one)");

        // Unwrapped, a command is one line, so a copy of it runs.
        let plain = render(Style::PLAIN.verbose(true), |d| d.item(&item));
        assert!(
            plain.contains(&format!("$ {}\n", args.join(" "))),
            "{plain}"
        );
    }

    #[test]
    fn a_command_in_a_detail_starts_at_the_detail_column() {
        let item = Item::new(Kind::Refused, "user _faber")
            .detail("_faber can authenticate. Lock it with")
            .detail_command(
                [
                    "sudo",
                    "/usr/bin/dscl",
                    ".",
                    "-delete",
                    "/Users/<_faber>",
                    "AuthenticationAuthority",
                ]
                .map(String::from)
                .to_vec(),
            )
            .detail("Setup does not adopt an account of another shape.");
        let out = render(
            Style {
                width: Some(100),
                ..WIDE
            },
            |d| d.item(&item),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(col_of(lines[0], "_faber can"), DETAIL_COL);
        assert_eq!(col_of(lines[1], "$ sudo"), DETAIL_COL);
        assert!(lines[1].ends_with(" \\"), "{out}");
        assert_eq!(lines[2].len() - lines[2].trim_start().len(), DETAIL_COL + 4);
        assert!(lines[2].ends_with("AuthenticationAuthority"), "{out}");
        assert_eq!(col_of(lines[3], "Setup does"), DETAIL_COL);
    }

    #[test]
    fn counts_print_every_kind_present_and_refused_always() {
        let counts = Counts {
            create: 29,
            change: 8,
            warning: 3,
            ok: 4,
            ..Counts::default()
        };
        let out = render(WIDE, |d| d.summary(2, &counts, None));
        assert_eq!(
            out,
            "  + 29 to create   ~ 8 to change   ! 3 warnings   x 0 refused\n"
        );
        let out = render(WIDE, |d| {
            d.summary(0, &Counts::of([Kind::Warning]), Some("--verbose"))
        });
        assert_eq!(out, "! 1 warning   x 0 refused   (--verbose)\n");

        let out = render(WIDE, |d| d.section("accounts", &counts));
        assert_eq!(
            out,
            "accounts                                      + 29 create   ~ 8 change   ! 3 warnings   ok 4\n"
        );
        assert_eq!(col_of(&out, "+"), COUNTS_COL);
        let out = render(WIDE, |d| {
            d.section("preflight", &Counts::of([Kind::Ok, Kind::Ok]))
        });
        assert_eq!(
            out,
            format!("preflight{}ok 2\n", " ".repeat(COUNTS_COL - 9))
        );
    }

    #[test]
    fn colour_only_reinforces_the_word_and_uses_basic_codes() {
        let coloured = Style {
            color: true,
            ..WIDE
        };
        let out = render(coloured, |d| {
            d.title("faber setup");
            d.section("accounts", &Counts::of(Kind::ALL));
            d.summary(2, &Counts::of(Kind::ALL), Some("--verbose"));
            for kind in Kind::ALL {
                d.item(&Item::new(kind, "subject").detail("detail"));
                d.glance(kind, "sentence");
            }
        });
        assert!(out.contains('\x1b'));
        let plain = render(WIDE, |d| {
            d.title("faber setup");
            d.section("accounts", &Counts::of(Kind::ALL));
            d.summary(2, &Counts::of(Kind::ALL), Some("--verbose"));
            for kind in Kind::ALL {
                d.item(&Item::new(kind, "subject").detail("detail"));
                d.glance(kind, "sentence");
            }
        });
        // Stripping the codes gives the plain text back: colour adds nothing
        // a reader without it misses.
        assert_eq!(anstream::adapter::strip_str(&out).to_string(), plain);
        for line in out.lines().filter(|l| l.contains('\x1b')) {
            assert!(
                Kind::ALL.iter().any(|k| line.contains(k.word()))
                    || line.contains("faber setup")
                    || line.starts_with("\x1b[1maccounts"),
                "a coloured line with no word: {line:?}"
            );
        }
        // Only SGR codes for the eight basic colours, bold, dim and reset.
        for seq in out.split('\x1b').skip(1) {
            let code = &seq[..seq.find('m').unwrap() + 1];
            assert!(
                matches!(
                    code,
                    "[0m" | "[1m" | "[2m" | "[31m" | "[32m" | "[33m" | "[36m"
                ),
                "{code:?}"
            );
        }
    }

    #[test]
    fn ok_is_never_coloured() {
        let out = render(
            Style {
                color: true,
                ..WIDE
            },
            |d| d.item(&Item::new(Kind::Ok, "x")),
        );
        assert!(!out.contains('\x1b'), "{out:?}");
    }

    #[test]
    fn every_string_is_sanitized_and_kept_to_one_line() {
        let evil = "a\x1b]52;c;aGk=\x07b\u{202e}c\nfake line";
        let out = render(
            Style {
                color: true,
                ..WIDE
            },
            |d| {
                d.title(evil);
                d.fact("operator", evil);
                d.item(
                    &Item::new(Kind::Create, evil)
                        .detail(evil)
                        .command(Command::of(vec![evil.into()]).note(evil)),
                );
                d.glance(Kind::Create, evil);
                d.section(evil, &Counts::default());
            },
        );
        let stripped = anstream::adapter::strip_str(&out).to_string();
        assert!(!stripped.contains('\x1b'), "{stripped:?}");
        assert!(!out.contains('\u{202e}'));
        assert!(!out.lines().any(|l| l.starts_with("fake")), "{out}");
    }

    #[test]
    fn the_width_is_the_terminals_clamped() {
        assert_eq!(clamp_width(Some(120), Some("70")), 100);
        assert_eq!(clamp_width(Some(40), None), 60);
        assert_eq!(clamp_width(Some(72), None), 72);
        assert_eq!(clamp_width(None, Some("90")), 90);
        assert_eq!(clamp_width(Some(0), Some("junk")), 80);
        assert_eq!(clamp_width(None, None), 80);
    }

    #[test]
    fn a_fact_wraps_under_its_value() {
        let out = render(
            Style {
                width: Some(60),
                ..WIDE
            },
            |d| {
                d.fact(
                    "layout",
                    "SYSTEM: state in /Library/Application Support/faber, logs in /Library/Logs/faber",
                )
            },
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(col_of(lines[0], "SYSTEM"), 12);
        for line in &lines[1..] {
            assert_eq!(line.len() - line.trim_start().len(), 12, "{out}");
        }
    }
}
