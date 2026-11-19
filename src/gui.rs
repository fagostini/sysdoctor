//! `sysdoctor gui`: GTK4 window with the full report, fix buttons, ignore, cleanup and copy-report.
use crate::finding::{human, printable, sh_quote, Finding, Fix, Severity};
use crate::{config, report};
use gtk::{gio, glib};
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use vte4::prelude::*;
use std::path::PathBuf;
use std::rc::Rc;

type Checks = fn() -> (Vec<Finding>, Vec<Finding>);
type Cleanups = fn() -> Vec<Finding>;

pub const APP_ID: &str = "io.github.sysdoctor";

const CSS: &str = ".crit { color: #e01b24; } .warn { color: #e5a50a; } .ok { color: #2ec27e; } .title { font-weight: bold; } .cmd { font-family: monospace; background: alpha(currentColor, 0.08); border: 1px solid alpha(currentColor, 0.15); border-radius: 6px; padding: 6px 8px; } .ignored { opacity: 0.6; } checkbutton check:not(:checked) { outline: 1px solid alpha(currentColor, 0.55); outline-offset: -2px; border-radius: 3px; } .caption { font-size: smaller; font-weight: bold; opacity: 0.7; }";

#[derive(Clone)]
struct Ui {
    window: gtk::ApplicationWindow,
    health: gtk::Box,
    clean: gtk::Box,
    clean_bar: gtk::Box,
    status: gtk::Label,
    spinner: gtk::Spinner,
    refresh: gtk::Button,
    report: Rc<RefCell<String>>,
    term: vte4::Terminal,
    term_pane: gtk::Box,
    running: Rc<Cell<bool>>,
    /// Ticks the user changed, by "id title", kept across refreshes.
    picked: Rc<RefCell<std::collections::HashMap<String, bool>>>,
    checks: Checks,
    cleanups: Cleanups,
}

/// GApplication makes it single-instance: a second launch raises the existing window.
pub fn run(checks: Checks, cleanups: Cleanups) -> i32 {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| match app.active_window() {
        Some(w) => w.present(),
        None => build(app, checks, cleanups),
    });
    app.run_with_args(&["sysdoctor"]).value() // clap already parsed argv
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).selectable(true).build()
}

fn vbox(spacing: i32, margin: i32) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    b.set_margin_top(margin);
    b.set_margin_bottom(margin);
    b.set_margin_start(margin);
    b.set_margin_end(margin);
    b
}

fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

fn sev_class(s: Severity) -> (&'static str, &'static str) {
    match s {
        Severity::Crit => ("dialog-error-symbolic", "crit"),
        Severity::Warn => ("dialog-warning-symbolic", "warn"),
        Severity::Ok => ("emblem-ok-symbolic", "ok"),
    }
}

fn build(app: &gtk::Application, checks: Checks, cleanups: Cleanups) {
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    if let Some(d) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    let window = gtk::ApplicationWindow::builder().application(app).title("sysdoctor").default_width(780).default_height(680).build();
    window.set_icon_name(Some("utilities-system-monitor"));

    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Check again"));
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy privacy-scrubbed report (Markdown)"));
    let spinner = gtk::Spinner::new();
    let stack = gtk::Stack::new();
    let header = gtk::HeaderBar::new();
    header.pack_start(&refresh);
    header.pack_start(&spinner);
    header.pack_end(&copy);
    header.set_title_widget(Some(&gtk::StackSwitcher::builder().stack(&stack).build()));
    window.set_titlebar(Some(&header));

    // header: stays put above the scrolling list
    let page = |name: &str, title: &str, header: &gtk::Box| {
        let b = vbox(10, 14);
        let sc = gtk::ScrolledWindow::builder().child(&b).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.append(header);
        outer.append(&sc);
        stack.add_titled(&outer, Some(name), title);
        b
    };
    let clean_bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    clean_bar.set_margin_start(14);
    clean_bar.set_margin_end(14);
    clean_bar.set_margin_top(10);
    let (health, clean) = (page("health", "Health", &gtk::Box::new(gtk::Orientation::Vertical, 0)), page("clean", "Cleanup", &clean_bar));
    let status = gtk::Label::builder().xalign(0.0).margin_start(14).margin_end(14).margin_top(6).margin_bottom(6).build();
    status.add_css_class("dim-label");
    status.set_hexpand(true);
    let version = gtk::Label::builder().label(format!("sysdoctor {}", crate::finding::VERSION)).margin_end(14).build();
    version.add_css_class("dim-label");
    version.set_selectable(true);
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    footer.append(&status);
    footer.append(&version);
    let term = vte4::Terminal::builder().vexpand(true).scrollback_lines(5000).build();
    term.set_size_request(-1, 160);
    let term_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let term_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let term_title = gtk::Label::builder().label("Terminal").xalign(0.0).hexpand(true).margin_start(10).build();
    term_title.add_css_class("caption");
    let hide = gtk::Button::from_icon_name("window-close-symbolic");
    hide.add_css_class("flat");
    hide.set_tooltip_text(Some("Hide terminal"));
    term_bar.append(&term_title);
    term_bar.append(&hide);
    term_pane.append(&term_bar);
    term_pane.append(&term);
    term_pane.set_visible(false);
    let paned = gtk::Paned::builder().orientation(gtk::Orientation::Vertical).start_child(&stack).end_child(&term_pane).vexpand(true).build();
    paned.set_resize_end_child(false);
    paned.set_position(400);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&paned);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&footer);
    window.set_child(Some(&root));

    let ui = Ui { window, health, clean, clean_bar, status, spinner, refresh, report: Rc::default(), term, term_pane, running: Rc::default(), picked: Rc::default(), checks, cleanups };
    let u = ui.clone();
    hide.connect_clicked(move |_| u.term_pane.set_visible(u.running.get()));
    let u = ui.clone();
    ui.term.connect_child_exited(move |t, code| {
        u.running.set(false);
        let msg = if code == 0 { "\r\n\x1b[32m✔ done\x1b[0m\r\n" } else { "\r\n\x1b[31m✖ failed\x1b[0m\r\n" };
        t.feed(msg.as_bytes());
        u.reload();
    });
    let u = ui.clone();
    ui.refresh.connect_clicked(move |_| u.reload());
    let u = ui.clone();
    copy.connect_clicked(move |_| {
        u.window.clipboard().set_text(&u.report.borrow());
        u.status.set_text("Report copied to clipboard");
    });
    ui.window.present();
    ui.reload();
}

impl Ui {
    fn busy(&self, msg: &str) {
        self.refresh.set_sensitive(false);
        self.spinner.start();
        self.status.set_text(msg);
    }

    fn reload(&self) {
        self.busy("Checking…");
        // drop the old cleanup list right away: its items may be gone already
        clear(&self.clean_bar);
        clear(&self.clean);
        let wait = label("Scanning for reclaimable space…");
        wait.set_selectable(false);
        wait.add_css_class("dim-label");
        self.clean.append(&wait);
        let ui = self.clone();
        glib::spawn_future_local(async move {
            // both start now; cleanup walks $HOME and is slower, so health shows first
            let (health, clean) = (gio::spawn_blocking(ui.checks), gio::spawn_blocking(ui.cleanups));
            let (f, ignored) = health.await.unwrap_or_default();
            *ui.report.borrow_mut() = report::report(&f);
            let n = ui.render_health(f, ignored);
            ui.status.set_text("Scanning for reclaimable space…");
            ui.render_clean(clean.await.unwrap_or_default());
            let when = glib::DateTime::now_local().and_then(|t| t.format("%H:%M")).map(|s| s.to_string()).unwrap_or_default();
            let probs = match n {
                0 => "No problems".to_string(),
                1 => "1 problem".to_string(),
                n => format!("{n} problems"),
            };
            ui.status.set_text(&format!("{probs} · last checked {when}"));
            ui.spinner.stop();
            ui.refresh.set_sensitive(true);
        });
    }

    /// One finding as a framed card. `pick`: cleanup checkbox in place of the severity icon.
    /// `ignored`: dimmed, with "Stop ignoring" instead of Run/Ignore.
    fn card(&self, f: &Finding, pick: Option<&gtk::CheckButton>, ignored: bool) -> gtk::Frame {
        let body = vbox(6, 10);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        match pick {
            Some(c) => top.append(c),
            None => {
                let (icon, class) = sev_class(f.severity);
                let img = gtk::Image::from_icon_name(icon);
                img.add_css_class(class);
                top.append(&img);
            }
        }
        if let Some(b) = f.bytes {
            let size = gtk::Label::new(Some(&human(b)));
            size.set_width_chars(7);
            size.set_xalign(1.0);
            size.add_css_class("title");
            top.append(&size);
        }
        let title = label(&f.title);
        title.set_selectable(false); // selectable text grabs focus on reload and shows up all-selected
        title.set_hexpand(true);
        title.add_css_class("title");
        top.append(&title);
        if ignored {
            let b = gtk::Button::with_label("Stop ignoring");
            b.set_valign(gtk::Align::Center);
            let (ui, f) = (self.clone(), f.clone());
            b.connect_clicked(move |_| match config::unignore(&f) {
                Ok(_) => ui.reload(),
                Err(e) => ui.status.set_text(&format!("unignore failed: {e}")),
            });
            top.append(&b);
        } else if pick.is_none() {
            if f.fix.is_some() {
                let b = gtk::Button::with_label("Run…");
                b.add_css_class("suggested-action");
                b.set_valign(gtk::Align::Center);
                let (ui, f) = (self.clone(), f.clone());
                b.connect_clicked(move |_| {
                    let (ui, f) = (ui.clone(), f.clone());
                    glib::spawn_future_local(async move { ui.apply(vec![f]).await });
                });
                top.append(&b);
            }
            let b = gtk::Button::with_label("Ignore");
            b.set_valign(gtk::Align::Center);
            b.set_tooltip_text(Some(&format!("Mute all \"{}\" findings (undo: sysdoctor ignore --remove {})", f.id, f.id)));
            let (ui, id) = (self.clone(), f.id);
            b.connect_clicked(move |_| match config::add_ignore(id) {
                Ok(()) => ui.reload(),
                Err(e) => ui.status.set_text(&format!("ignore failed: {e}")),
            });
            top.append(&b);
        }
        body.append(&top);
        if !f.detail.is_empty() {
            let d = label(&f.detail);
            d.add_css_class("dim-label");
            if f.detail.lines().count() > 3 {
                body.append(&gtk::Expander::builder().label("Details").child(&d).build());
            } else {
                body.append(&d);
            }
        }
        let (caption, code) = match &f.fix {
            Some(Fix::Command(c)) => ("Runs", c.clone()),
            Some(Fix::Trash(p)) => {
                let mut list: Vec<String> = p.iter().take(5).map(|p| p.display().to_string()).collect();
                if p.len() > 5 {
                    list.push(format!("…and {} more", p.len() - 5));
                }
                ("Moves to trash", list.join("\n"))
            }
            None => ("", String::new()),
        };
        if !code.is_empty() {
            let cap = label(caption);
            cap.add_css_class("caption");
            let l = label(&code);
            l.add_css_class("cmd");
            l.set_lines(6);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            l.set_tooltip_text(Some(&code));
            body.append(&cap);
            body.append(&l);
        }
        let frame = gtk::Frame::builder().child(&body).build();
        if ignored {
            frame.add_css_class("ignored");
        }
        frame
    }

    /// Returns the number of problems shown.
    fn render_health(&self, mut f: Vec<Finding>, ignored: Vec<Finding>) -> usize {
        clear(&self.health);
        f.sort_by_key(|f| std::cmp::Reverse(f.severity));
        let (problems, ok): (Vec<_>, Vec<_>) = f.into_iter().partition(|f| f.severity != Severity::Ok);
        if problems.is_empty() {
            let l = gtk::Label::new(Some("All checks passed"));
            l.add_css_class("title");
            l.add_css_class("ok");
            l.set_margin_top(20);
            self.health.append(&l);
        }
        for p in &problems {
            self.health.append(&self.card(p, None, false));
        }
        let passing = vbox(2, 6);
        for o in &ok {
            passing.append(&label(&format!("✓  {}", o.title)));
        }
        let title = format!("Passing checks ({})", ok.len());
        self.health.append(&gtk::Expander::builder().label(title).child(&passing).margin_top(8).build());
        if !ignored.is_empty() {
            let list = vbox(8, 6);
            for i in &ignored {
                list.append(&self.card(i, None, true));
            }
            let title = format!("Ignored ({})", ignored.len());
            self.health.append(&gtk::Expander::builder().label(title).child(&list).expanded(true).build());
        }
        problems.len()
    }

    fn render_clean(&self, mut items: Vec<Finding>) {
        clear(&self.clean);
        items.sort_by_key(|f| std::cmp::Reverse(f.bytes.unwrap_or(0)));
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let all: u64 = items.iter().filter_map(|f| f.bytes).sum();
        let total = label("");
        total.set_selectable(false);
        total.set_hexpand(true);
        let go = gtk::Button::with_label("Clean selected…");
        go.add_css_class("suggested-action");
        bar.append(&total);
        bar.append(&go);
        clear(&self.clean_bar);
        self.clean_bar.append(&bar);
        let mut picks = vec![];
        for f in items {
            // regenerable (Ok) pre-ticked; "review" (Warn) items are opt-in
            let key = format!("{} {}", f.id, f.title);
            let on = self.picked.borrow().get(&key).copied().unwrap_or(f.severity == Severity::Ok);
            let c = gtk::CheckButton::builder().active(on).valign(gtk::Align::Center).build();
            let picked = self.picked.clone();
            c.connect_toggled(move |c| {
                picked.borrow_mut().insert(key.clone(), c.is_active());
            });
            self.clean.append(&self.card(&f, Some(&c), false));
            picks.push((c, f));
        }
        let picks = Rc::new(picks);
        let update = {
            let (picks, total, go) = (picks.clone(), total.clone(), go.clone());
            move || {
                let sel: u64 = picks.iter().filter(|(c, _)| c.is_active()).filter_map(|(_, f)| f.bytes).sum();
                total.set_markup(&format!("Selected: <b>{}</b> of {} reclaimable. Unticked items need review first.", human(sel), human(all)));
                go.set_sensitive(picks.iter().any(|(c, _)| c.is_active()));
            }
        };
        update();
        for (c, _) in picks.iter() {
            let update = update.clone();
            c.connect_toggled(move |_| update());
        }
        let ui = self.clone();
        go.connect_clicked(move |_| {
            let sel: Vec<Finding> = picks.iter().filter(|(c, _)| c.is_active()).map(|(_, f)| f.clone()).collect();
            let ui = ui.clone();
            glib::spawn_future_local(async move { ui.apply(sel).await });
        });
    }

    /// Confirm, trash paths in-process, run commands in one terminal window (sudo prompts there), then re-check.
    async fn apply(&self, items: Vec<Finding>) {
        if self.running.get() {
            self.status.set_text("A command is still running in the terminal");
            return;
        }
        let paths: Vec<PathBuf> = items.iter().filter_map(|f| if let Some(Fix::Trash(p)) = &f.fix { Some(p.clone()) } else { None }).flatten().collect();
        let cmds: Vec<&str> = items.iter().filter_map(|f| if let Some(Fix::Command(c)) = &f.fix { Some(c.as_str()) } else { None }).collect();
        if paths.is_empty() && cmds.is_empty() {
            self.status.set_text("Nothing selected");
            return;
        }
        let mut detail = String::new();
        if !paths.is_empty() {
            detail += "Move to trash:\n";
            paths.iter().take(8).for_each(|p| detail += &format!("  {}\n", p.display()));
            if paths.len() > 8 {
                detail += &format!("  …and {} more\n", paths.len() - 8);
            }
        }
        if !cmds.is_empty() {
            detail += "\nRun:\n";
            // in full: what is confirmed must be exactly what runs
            cmds.iter().for_each(|c| detail += &format!("  {}\n", printable(c)));
        }
        let ask = gtk::AlertDialog::builder()
            .modal(true)
            .message("Apply fix?")
            .detail(detail.trim_end())
            .buttons(["Cancel", "Apply"])
            .cancel_button(0)
            .default_button(1)
            .build();
        if ask.choose_future(Some(&self.window)).await != Ok(1) {
            return;
        }
        self.busy("Applying…");
        if !paths.is_empty() {
            let n = paths.len();
            match gio::spawn_blocking(move || trash::delete_all(&paths).map_err(|e| e.to_string())).await {
                Ok(Ok(())) => self.status.set_text(&format!("Trashed {n} paths. Empty the trash to free the space.")),
                Ok(Err(e)) => self.status.set_text(&format!("trash failed: {e}")),
                Err(_) => {}
            }
        }
        if cmds.is_empty() {
            self.reload();
            return;
        }
        // echo each command, stop at the first failure; child-exited re-checks
        let script = cmds.iter().map(|c| format!("printf '\\033[1;36m$ %s\\033[0m\\n' {} && {c}", sh_quote(&printable(c)))).collect::<Vec<_>>().join(" && ");
        self.term_pane.set_visible(true);
        self.running.set(true);
        self.status.set_text("Running in terminal below…");
        let ui = self.clone();
        self.term.spawn_async(
            vte4::PtyFlags::DEFAULT,
            None,
            &["/bin/sh", "-c", &script],
            &["PAGER=cat", "SYSTEMD_PAGER=cat"], // output into scrollback, no "press q"
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r {
                    ui.running.set(false);
                    ui.status.set_text(&format!("could not start: {e}"));
                    ui.reload();
                }
            },
        );
        self.term.grab_focus();
    }
}
