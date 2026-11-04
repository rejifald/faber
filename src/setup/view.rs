//! `faber setup`'s plan as printed (ADR-0050): the verdict first, then the
//! detail.
//!
//! 1. a header and the facts the plan rests on
//! 2. the counts, warnings and refusals included
//! 3. every refused item, in full
//! 4. at a glance: what changes on the machine, in words ([`Section::glance`])
//! 5. the sections, each with its counts, one line per item
//! 6. the counts again, and one trailer saying what happens next
//!
//! An item is one or more steps that share a key ([`super::plan::Shown`]): a
//! role account is one line, not four. Counts count items. The renderer never
//! decides what a step means; it lays out what the plan says, through
//! [`crate::out`], which sanitizes every string it is given.

use std::path::Path;

use crate::out::{Command, Counts, Doc, Item, Kind, Para, Style};
use crate::platform::layout::Os;

use super::plan::{Action, HostState, Section, Status, Step};

/// What happens after the plan, which decides its last line (ADR-0050,
/// section 1, item 7): the first that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End<'a> {
    /// Without `sudo`: read-only probes, and the command that applies it.
    Preview { exe: &'a Path },
    /// `--dry-run`: nothing changed.
    DryRun,
    /// A refusal under `sudo`: nothing was changed, and no prompt.
    Refused,
    /// Nothing to change: the closing text follows, and no prompt.
    Done,
    /// The `Proceed? [y/N]` prompt follows.
    Prompt,
}

/// One printed line of the plan: the steps it stands for.
struct Group<'a> {
    steps: Vec<&'a Step>,
}

impl Group<'_> {
    fn first(&self) -> &Step {
        self.steps[0]
    }

    /// The item's kind: a refusal or a warning in it wins; a mix of `ok` and
    /// `create` (an account that exists but is not yet hidden) is a change.
    fn kind(&self) -> Kind {
        let has = |st: Status| self.steps.iter().any(|s| s.status == st);
        if has(Status::Refused) {
            Kind::Refused
        } else if has(Status::Skipped) {
            Kind::Warning
        } else if has(Status::WillChange) || (has(Status::WillCreate) && has(Status::Ok)) {
            Kind::Change
        } else if has(Status::WillCreate) {
            Kind::Create
        } else {
            Kind::Ok
        }
    }

    /// Folded into its section's `ok` count in the default view.
    fn folded(&self) -> bool {
        self.kind() == Kind::Ok && !self.steps.iter().any(|s| s.shown.listed)
    }

    fn name(&self) -> String {
        let st = self.first();
        st.shown
            .name
            .clone()
            .or_else(|| st.shown.item.clone())
            .unwrap_or_else(|| st.subject.clone())
    }

    /// The item as `out` lays it out: the default detail is the steps'
    /// briefs (a refusal's or a warning's whole detail), `--verbose` adds
    /// each step's full detail and its commands.
    fn item(&self) -> Item {
        let mut item = Item::new(self.kind(), self.name());
        let urgent: Vec<&Step> = self
            .steps
            .iter()
            .copied()
            .filter(|s| matches!(s.status, Status::Refused | Status::Skipped))
            .collect();
        if !urgent.is_empty() {
            for st in urgent {
                // The kind already says `warning`.
                let text = st.detail.strip_prefix("warning: ").unwrap_or(&st.detail);
                item.detail.extend(paras(text));
            }
            item.verbose_detail = Some(item.detail.clone());
        } else {
            let briefs: Vec<String> = self
                .steps
                .iter()
                .map(|s| s.brief_text())
                .filter(|b| !b.trim().is_empty())
                .collect();
            item = item.detail(briefs.join(", "));
            let mut verbose = item.detail.clone();
            for st in &self.steps {
                // A step shown under another name keeps its own subject in
                // the full view, so nothing the old output said is lost.
                let full = full_text(st).map(|f| match &st.shown.name {
                    Some(_) => format!("{}: {f}", st.subject),
                    None => f,
                });
                if self.steps.len() == 1 {
                    verbose = full.map_or(verbose, |f| paras(&f));
                } else if let Some(full) = full.filter(|f| *f != st.brief_text()) {
                    verbose.extend(paras(&full));
                }
            }
            item.verbose_detail = Some(verbose);
        }
        for st in &self.steps {
            item.commands.extend(commands(st));
        }
        item
    }
}

/// A step's detail as paragraphs: a line break in it (a child's output under
/// a refusal) starts a new one, and a command quoted in backticks (a
/// refusal's fix) is a command of its own, at the detail column, so it is
/// never split mid-argument (ADR-0050, section 5).
fn paras(text: &str) -> Vec<Para> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut rest = line;
        let mut after_command = false;
        loop {
            // The first span in backticks that holds a command: one word
            // (`--verbose`) stays in the sentence.
            let mut from = 0;
            let quoted = loop {
                let Some(open) = rest[from..].find('`').map(|at| at + from) else {
                    break None;
                };
                let Some(close) = rest[open + 1..].find('`').map(|at| at + open + 1) else {
                    break None;
                };
                let inner = &rest[open + 1..close];
                if inner.contains(' ') {
                    break Some((open, close, inner));
                }
                from = close + 1;
            };
            let Some((open, close, inner)) = quoted else {
                push_text(&mut out, rest, after_command);
                break;
            };
            let before = &rest[..open];
            // `cmd` and `cmd`: the joining word goes with the layout.
            if !(after_command && matches!(before.trim(), "" | "and" | "," | ", and")) {
                push_text(&mut out, before, after_command);
            }
            out.push(Para::Command(shell_words(inner)));
            after_command = true;
            rest = &rest[close + 1..];
        }
    }
    out
}

/// Text after a command starts a sentence of its own: `; setup does not`
/// reads `Setup does not`.
fn push_text(out: &mut Vec<Para>, text: &str, after_command: bool) {
    let mut text = text.trim();
    if after_command {
        text = text.trim_start_matches([';', ',', '.', ':']).trim_start();
    }
    if text.is_empty() {
        return;
    }
    let text = if after_command {
        let mut chars = text.chars();
        chars.next().map_or_else(String::new, |c| {
            c.to_uppercase().chain(chars).collect::<String>()
        })
    } else {
        text.to_string()
    };
    out.push(Para::Text(text));
}

/// A command's words, keeping a double-quoted argument whole.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in command.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                word.push(c);
            }
            ' ' if !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// What `--verbose` says about a step beyond its commands: its detail, less
/// the command it starts with. `None` when that leaves nothing.
fn full_text(st: &Step) -> Option<String> {
    let text = match &st.action {
        Some(Action::Run(cmd)) => st.after_command(cmd).to_string(),
        Some(Action::LockPassword { .. }) => String::new(),
        _ => st.detail.clone(),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// The commands a step runs, as `--verbose` lists them.
fn commands(st: &Step) -> Vec<Command> {
    let args = |cmd: &super::tools::Cmd| {
        std::iter::once(cmd.tool.path().to_string())
            .chain(cmd.args.iter().map(|a| super::tools::quote(a)))
            .collect::<Vec<_>>()
    };
    match &st.action {
        Some(Action::Run(cmd)) => vec![Command::of(args(cmd))],
        Some(Action::LockPassword { name }) => {
            let [set, delete] = super::accounts::lock_password(name);
            vec![
                Command::of(args(&set)),
                Command::of(args(&delete)).note("only if it has one; then checked, as for _www"),
            ]
        }
        _ => Vec::new(),
    }
}

/// The plan's items, section by section in [`Section`] order: within a
/// section refused items first, then warnings, then the rest in plan order.
fn sections(steps: &[Step]) -> Vec<(Section, Vec<Group<'_>>)> {
    let mut out: Vec<(Section, Vec<Group<'_>>)> = Vec::new();
    let mut order: Vec<Section> = steps.iter().map(|s| s.section).collect();
    order.sort();
    order.dedup();
    for section in order {
        let mut groups: Vec<Group<'_>> = Vec::new();
        for st in steps.iter().filter(|s| s.section == section) {
            let joined = st.shown.item.as_ref().and_then(|key| {
                groups
                    .iter_mut()
                    .find(|g| g.first().shown.item.as_ref() == Some(key))
            });
            match joined {
                Some(group) => group.steps.push(st),
                None => groups.push(Group { steps: vec![st] }),
            }
        }
        let rank = |g: &Group<'_>| match g.kind() {
            Kind::Refused => 0,
            Kind::Warning => 1,
            _ => 2,
        };
        // A stable sort keeps plan order within each rank.
        groups.sort_by_key(rank);
        out.push((section, groups));
    }
    out
}

/// The plan, as `sudo faber setup` prints it, ending with `end`'s trailer.
pub fn render(s: &HostState, steps: &[Step], style: Style, end: End<'_>) -> String {
    let sections = sections(steps);
    let mut total = Counts::default();
    for (_, groups) in &sections {
        for g in groups {
            total.add(g.kind());
        }
    }
    let mut doc = Doc::new(style);
    let host = match s.os {
        Os::MacOs => "this Mac",
        Os::Linux => "this host",
    };
    doc.title(&format!(
        "faber setup · plan for {host} · nothing has changed yet"
    ));
    doc.blank();
    match &s.operator {
        Ok(op) if style.verbose => doc.fact(
            "operator",
            &format!("{} (uid {}), home {}", op.name, op.uid, op.home.display()),
        ),
        Ok(op) => doc.fact("operator", &format!("{} (uid {})", op.name, op.uid)),
        Err(_) => doc.fact("operator", "unknown (see preflight)"),
    }
    doc.fact("binary", &format!("{} {}", s.exe_path.display(), s.version));
    doc.fact(
        "layout",
        &format!(
            "SYSTEM: state in {}, logs in {}",
            s.layout.state_dir.display(),
            s.layout.log_dir.display()
        ),
    );
    doc.blank();
    let settled = !total.any_change() && total.warning == 0 && total.refused == 0;
    if settled {
        doc.text(2, "nothing to change: this host is set up");
    } else {
        doc.summary(2, &total, None);
    }

    if total.refused > 0 {
        doc.blank();
        doc.heading("Refused: setup will not run until this is fixed");
        for (_, groups) in &sections {
            for g in groups.iter().filter(|g| g.kind() == Kind::Refused) {
                doc.item(&g.item());
            }
        }
    }

    let mut glance: Vec<(Kind, String)> = sections
        .iter()
        .flat_map(|(section, groups)| {
            let steps: Vec<&Step> = groups.iter().flat_map(|g| g.steps.clone()).collect();
            section.glance(s, &steps)
        })
        .collect();
    // Warnings first, as within a section: the line a reviewer must not
    // miss leads the block. A stable sort keeps plan order otherwise.
    glance.sort_by_key(|(kind, _)| *kind != Kind::Warning);
    if !glance.is_empty() {
        doc.blank();
        doc.heading("At a glance");
        for (kind, text) in &glance {
            doc.glance(*kind, text);
        }
    }

    for (section, groups) in &sections {
        doc.blank();
        doc.section(section.title(), &Counts::of(groups.iter().map(Group::kind)));
        for g in groups.iter().filter(|g| style.verbose || !g.folded()) {
            doc.item(&g.item());
        }
    }

    doc.blank();
    let hint = (!style.verbose).then_some("--verbose: commands and modes");
    if settled {
        doc.text(0, "nothing to change: this host is set up");
    } else {
        doc.summary(0, &total, hint);
    }
    let refused = total.refused;
    let would_stop = |doc: &mut Doc| {
        if refused > 0 {
            doc.text(
                0,
                &format!(
                    "A real run would stop at the {refused} refused item{} above.",
                    if refused == 1 { "" } else { "s" }
                ),
            );
        }
    };
    match end {
        End::Preview { exe } => {
            doc.text(
                0,
                "This preview used read-only probes; nothing changed. Re-run with sudo to \
                 apply it:",
            );
            doc.text(2, &format!("sudo {} setup", exe.display()));
            would_stop(&mut doc);
        }
        End::DryRun => {
            doc.text(0, "--dry-run: nothing changed.");
            would_stop(&mut doc);
        }
        End::Refused => doc.verdict(Kind::Refused, refused, ": nothing was changed"),
        End::Done | End::Prompt => {}
    }
    doc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::out::{DETAIL_COL, SUBJECT_COL};
    use crate::platform::layout::{FABER_ACCOUNT, FABER_VM_ACCOUNT};
    use crate::setup::plan::{plan, tests::fresh_mac, tests::set_up_mac};

    const WIDE: Style = Style {
        color: false,
        width: Some(100),
        verbose: false,
    };

    fn shown(s: &HostState, style: Style, end: End<'_>) -> String {
        render(s, &plan(s), style, end)
    }

    /// `out` against the golden file `name` beside this one. With
    /// `FABER_BLESS=1` the file is rewritten instead, to review in the diff.
    /// A role account's directory-service node is written `/Users/<_faber>`,
    /// as in ADR-0050: the privacy gate takes `/Users/` only before a few
    /// generic names.
    fn golden(name: &str, out: &str) {
        let mut out = out.to_string();
        for name in [FABER_VM_ACCOUNT, FABER_ACCOUNT] {
            out = out.replace(&format!("/Users/{name} "), &format!("/Users/<{name}> "));
        }
        let out = &out;
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/setup")
            .join(name);
        if std::env::var_os("FABER_BLESS").is_some() {
            std::fs::write(&path, out).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path).unwrap();
        assert_eq!(*out, want, "\n{out}");
    }

    /// ADR-0050's "After, default view", for the same fixture. Where the
    /// fixture differs from the ADR's mock, the mock's text is the one that
    /// was hand-written; this is what the plan code says.
    #[test]
    fn a_fresh_mac_renders_summary_first() {
        let s = fresh_mac();
        golden("view_fresh_mac.txt", &shown(&s, WIDE, End::Prompt));
        golden(
            "view_fresh_mac_verbose.txt",
            &shown(&s, WIDE.verbose(true), End::Prompt),
        );
    }

    #[test]
    fn a_set_up_mac_renders_nothing_to_change() {
        let s = set_up_mac();
        golden("view_set_up_mac.txt", &shown(&s, WIDE, End::Done));
    }

    /// S1 on a host set up before the operator group: the move out of
    /// `_faber`'s group, the regrouping and the restart, as the operator
    /// reads them before saying yes.
    #[test]
    fn a_host_set_up_before_the_operator_group_renders_the_move() {
        let s = crate::setup::plan::tests::set_up_before_the_operator_group();
        golden("view_operator_group_mac.txt", &shown(&s, WIDE, End::Prompt));
    }

    #[test]
    fn a_fresh_linux_host_renders_its_own_plan() {
        let s = crate::setup::plan::tests::fresh_linux();
        golden("view_fresh_linux.txt", &shown(&s, WIDE, End::Prompt));
    }

    /// The ADR's refused example, as a preview prints it: the refusal right
    /// after the counts, and the preview's trailer.
    #[test]
    fn a_refused_preview_renders_the_refusal_first() {
        let mut s = fresh_mac();
        crate::setup::plan::tests::with_an_authenticating_faber(&mut s);
        let exe = s.exe_path.clone();
        golden(
            "view_refused_mac.txt",
            &shown(&s, WIDE, End::Preview { exe: &exe }),
        );
    }

    #[test]
    fn piped_output_is_one_line_per_item_and_says_the_same() {
        let s = fresh_mac();
        let wide = shown(&s, WIDE, End::Prompt);
        let piped = shown(&s, Style::PLAIN, End::Prompt);
        let words = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words(&wide), words(&piped));
        // Nothing wraps: a line at the detail column is a detail moved under
        // a long subject, so it follows its item's line and never another
        // detail line.
        let lines: Vec<&str> = piped.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.len() - line.trim_start().len() == DETAIL_COL {
                let prev = lines[i - 1];
                assert_eq!(
                    prev.len() - prev.trim_start().len(),
                    2,
                    "{prev:?}\n{line:?}"
                );
            }
        }
    }

    /// ADR-0050 section 5: every detail starts at the detail column, on the
    /// item's line or the next, in every fixture and in both views.
    #[test]
    fn every_detail_starts_at_the_detail_column() {
        for s in [fresh_mac(), set_up_mac()] {
            for style in [
                WIDE,
                WIDE.verbose(true),
                Style::PLAIN,
                Style::PLAIN.verbose(true),
            ] {
                let out = shown(&s, style, End::Prompt);
                for line in out.lines() {
                    let indent = line.len() - line.trim_start().len();
                    let body = line.trim_start();
                    let is_item =
                        indent == 2 && Kind::ALL.iter().any(|k| body.starts_with(&k.label()));
                    if is_item {
                        let rest = &line[SUBJECT_COL..];
                        let subject_end = rest.find("  ").map(|at| SUBJECT_COL + at);
                        if let Some(end) = subject_end {
                            let detail_at =
                                end + line[end..].len() - line[end..].trim_start().len();
                            assert_eq!(detail_at, DETAIL_COL, "{line:?}\n{out}");
                        }
                    } else if !body.is_empty() && indent > SUBJECT_COL && !body.starts_with('$') {
                        // A wrapped detail, or a detail on its own line, or a
                        // command's continuation or note.
                        assert!(
                            indent == DETAIL_COL
                                || indent == SUBJECT_COL + 2
                                || indent == SUBJECT_COL + 4
                                || indent == DETAIL_COL + 4,
                            "{line:?}\n{out}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn verbose_lists_every_step_and_its_commands() {
        let s = fresh_mac();
        let steps = plan(&s);
        let out = render(&s, &steps, Style::PLAIN.verbose(true), End::Prompt);
        for st in &steps {
            if let Some(Action::Run(cmd)) = &st.action {
                let line = format!("$ {}", cmd.display());
                assert!(out.contains(&line), "{line}\n{out}");
            }
            // Nothing today's output printed is dropped from the verbose view.
            if !matches!(
                st.action,
                Some(Action::Run(_)) | Some(Action::LockPassword { .. })
            ) {
                let detail = st.detail.strip_prefix("warning: ").unwrap_or(&st.detail);
                let words = crate::term::sanitize(detail)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(out.contains(&words), "{words}\n{out}");
            }
        }
        assert!(out.contains("home /Users/owner"));
        let quiet = render(&s, &steps, Style::PLAIN, End::Prompt);
        assert!(!quiet.contains("$ "), "{quiet}");
        assert!(!quiet.contains("_faber:_faber_operator 750"), "{quiet}");
    }

    #[test]
    fn the_counts_lead_and_close_and_count_warnings() {
        let s = fresh_mac();
        let out = shown(&s, Style::PLAIN, End::Prompt);
        let lines: Vec<&str> = out.lines().collect();
        let counts = lines.iter().find(|l| l.starts_with("  + ")).unwrap();
        assert!(
            counts.contains("! 4 warnings") && counts.contains("x 0 refused"),
            "{out}"
        );
        assert_eq!(
            lines.last().unwrap(),
            &format!("{}   (--verbose: commands and modes)", counts.trim())
        );
        let verbose = shown(&s, Style::PLAIN.verbose(true), End::Prompt);
        assert_eq!(verbose.lines().last().unwrap(), counts.trim());
    }

    #[test]
    fn each_trailer_says_what_happens_next() {
        let s = fresh_mac();
        let exe = Path::new("/usr/local/bin/faber");
        let preview = shown(&s, Style::PLAIN, End::Preview { exe });
        assert!(preview.ends_with(
            "This preview used read-only probes; nothing changed. Re-run with sudo to apply \
             it:\n  sudo /usr/local/bin/faber setup\n"
        ));
        let dry = shown(&s, Style::PLAIN, End::DryRun);
        assert!(dry.ends_with("--dry-run: nothing changed.\n"));
    }

    /// A refusal is listed in full right after the counts, and again first in
    /// its section; the trailer says nothing was changed.
    #[test]
    fn a_refused_plan_puts_the_refusal_first() {
        let mut s = fresh_mac();
        crate::setup::plan::tests::with_an_authenticating_faber(&mut s);
        let steps = plan(&s);
        assert!(steps.iter().any(|st| st.status == Status::Refused));
        let out = render(&s, &steps, Style::PLAIN, End::Refused);
        let lines: Vec<&str> = out.lines().collect();
        let counts = lines.iter().position(|l| l.starts_with("  + ")).unwrap();
        assert!(lines[counts].ends_with("x 1 refused"), "{out}");
        assert_eq!(
            lines[counts + 2],
            "  Refused: setup will not run until this is fixed"
        );
        assert!(
            lines[counts + 3].starts_with("  x refused  user _faber "),
            "{out}"
        );
        assert_eq!(lines.last().unwrap(), &"x 1 refused: nothing was changed");
        let section = lines
            .iter()
            .position(|l| l.starts_with("accounts"))
            .unwrap();
        assert!(
            lines[section + 1].starts_with("  x refused  user _faber "),
            "{out}"
        );
    }

    #[test]
    fn a_quoted_fix_is_a_command_of_its_own() {
        let p = paras(
            "it can log in. Lock it with `sudo /usr/bin/dscl . -delete \"/Users/ci y\" A` and \
             `sudo /usr/bin/dscl . -create /Users/ci Password \"*\"`; setup does not adopt it. \
             See `--verbose` or `faber engine stop`, then re-run",
        );
        assert_eq!(
            p,
            [
                Para::Text("it can log in. Lock it with".into()),
                Para::Command(
                    [
                        "sudo",
                        "/usr/bin/dscl",
                        ".",
                        "-delete",
                        "\"/Users/ci y\"",
                        "A"
                    ]
                    .map(String::from)
                    .to_vec()
                ),
                Para::Command(
                    [
                        "sudo",
                        "/usr/bin/dscl",
                        ".",
                        "-create",
                        "/Users/ci",
                        "Password",
                        "\"*\""
                    ]
                    .map(String::from)
                    .to_vec()
                ),
                Para::Text("Setup does not adopt it. See `--verbose` or".into()),
                Para::Command(["faber", "engine", "stop"].map(String::from).to_vec()),
                Para::Text("Then re-run".into()),
            ]
        );
    }

    #[test]
    fn colour_marks_only_the_kinds_and_keeps_their_words() {
        let s = fresh_mac();
        let coloured = shown(
            &s,
            Style {
                color: true,
                ..WIDE
            },
            End::Prompt,
        );
        let plain = shown(&s, WIDE, End::Prompt);
        assert!(coloured.contains("\x1b[32m+ create\x1b[0m"));
        assert_eq!(anstream::adapter::strip_str(&coloured).to_string(), plain);
    }

    #[test]
    fn every_section_prints_in_plan_order() {
        let s = fresh_mac();
        let out = shown(&s, Style::PLAIN, End::Prompt);
        let mut at = 0;
        for title in [
            "preflight",
            "accounts",
            "directories",
            "binaries",
            "per-user instances",
            "config and state",
            "log files",
            "setup record",
            "VMs (ADR-0026)",
            "service",
            "your old per-user files",
            "keep awake",
            "menu-bar app",
            "Linux VM image",
        ] {
            let found = out[at..]
                .find(&format!("\n{title} "))
                .or_else(|| out[at..].find(&format!("\n{title}\n")))
                .unwrap_or_else(|| panic!("{title} missing or out of order:\n{out}"));
            at += found + 1;
        }
    }
}
