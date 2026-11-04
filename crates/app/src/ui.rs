//! The window: the selected kid's screen on the left; their webcam, the four
//! toggles and one card per PC on the right.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use gtk4::{self as gtk, gdk, glib, graphene, pango, prelude::*};
use tutor_proto::{ABS_MAX, Input, Presence, Streams, ToStation};

use crate::{
    config::Config,
    settings::{self, Settings, StationSettings},
    media::{self, Me, Paintables, Playback},
    station::{Devices, Event, Link, Station},
};

/// Starting sidebar width; drag the divider to change it.
const SELECTED_FPS: u8 = 30;
const BACKGROUND_FPS: u8 = 5;
/// evdev KEY_ESC, as GDK reports it (X keycode = evdev + 8).
const ESC_KEYCODE: u32 = 9;

struct View {
    station: Arc<Station>,
    paint: Paintables,
    card: gtk::Button,
    title: gtk::Label,
    /// Problems that last until fixed (no headset, no camera); hidden when none.
    problems: gtk::Label,
    sub: gtk::Label,
    level: gtk::LevelBar,
    online: Cell<bool>,
    note: RefCell<String>,
    /// "My cam" is remembered per PC.
    dad_cam: Cell<bool>,
    /// `None` until the station first reports them.
    devices: RefCell<Option<Devices>>,
    /// Their camera reported an error and no frame has come since.
    camera_failed: Cell<bool>,
    presence: Cell<Presence>,
    link: Cell<Link>,
    /// Where my cam sits on their screen (`Streams::dad_cam_at`), per PC.
    dad_cam_at: Cell<(u16, u16)>,
    /// Index into `QUALITIES`, remembered per PC.
    quality: Cell<u32>,
}

/// Screen quality choices: label and the divisor for width and height;
/// 0 stops capturing their screen at all, so their games lose nothing.
const QUALITIES: [(&str, u8); 4] = [
    ("Full — reading, homework", 1),
    ("Half — playing together", 2),
    ("Low — bad connection", 3),
    ("Off — don't touch their FPS", 0),
];

struct Toggles {
    talk: gtk::ToggleButton,
    hear: gtk::ToggleButton,
    cam: gtk::ToggleButton,
    control: gtk::ToggleButton,
}

struct Ui {
    views: Vec<View>,
    me: Me,
    selected: Cell<usize>,
    talk: Cell<bool>,
    hear: Cell<bool>,
    control: Cell<bool>,
    screen: gtk::Picture,
    screen_scroll: gtk::ScrolledWindow,
    /// 1.0 = whole screen fits; up to 4.0.
    zoom: Cell<f64>,
    /// Last pointer position over the screen view, in its own coordinates.
    pointer: Cell<(f64, f64)>,
    webcam: gtk::Picture,
    /// Over the screen view while offline, locked or asleep.
    banner: gtk::Box,
    banner_text: gtk::Label,
    banner_wake: gtk::Button,
    /// Bottom-right of the screen view: round trip, fps, bitrate; turns
    /// amber when laggy and red when stalled.
    health: gtk::Label,
    status: gtk::Label,
    output: gtk::DropDown,
    volume: gtk::Scale,
    /// The PC whose volume Dad is moving the slider for, while he moves it: their
    /// echoes don't move it back, and a pending send can't land on another PC.
    volume_for: Cell<Option<usize>>,
    mic: gtk::DropDown,
    quality: gtk::DropDown,
    toggles: Toggles,
    paned: gtk::Paned,
    /// Last written settings, to skip writes that change nothing.
    saved: Rc<RefCell<Settings>>,
    /// Newest settings waiting for the scheduled write.
    pending: Rc<RefCell<Option<Settings>>>,
    /// Set while the UI updates toggles itself, so handlers ignore it.
    updating: Cell<bool>,
    held_keys: RefCell<BTreeSet<u16>>,
    esc_timer: RefCell<Option<glib::SourceId>>,
}

pub fn run(config: Config) -> Result<()> {
    gstreamer::init()?;
    gstgtk4::plugin_register_static()?;
    let app = gtk::Application::builder().application_id("dev.towles.Tutor").build();
    app.connect_activate(move |app| {
        if let Err(e) = build(app, &config) {
            tracing::error!(error = format!("{e:#}"), "app.start_failed");
            app.quit();
        }
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &gtk::Application, config: &Config) -> Result<()> {
    let saved = settings::load();
    let speakers = media::find_device("sinks", &config.me.speakers);
    let mut stations = Vec::new();
    let mut paints = Vec::new();
    for sc in &config.stations {
        let (playback, paint) = Playback::new(&sc.name, speakers.as_deref())?;
        stations.push(Station::new(sc.clone(), playback));
        paints.push(paint);
    }

    let me = {
        let (for_voice, for_cam) = (stations.clone(), stations.clone());
        Me::new(
            &config.me.mic,
            config.me.camera.clone(),
            move |pcm| {
                for s in &for_voice {
                    if s.streams().dad_voice {
                        s.send(ToStation::DadVoice(pcm.clone()));
                    }
                }
            },
            move |jpeg| {
                for s in &for_cam {
                    if s.streams().dad_cam {
                        s.send(ToStation::DadCam(jpeg.clone()));
                    }
                }
            },
        )?
    };

    // Left: their screen, with a banner over it while offline.
    let screen = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .build();
    let banner_text = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["title-2"]).build();
    let banner_wake = gtk::Button::builder().label("🔓 Wake & unlock").halign(gtk::Align::Center).css_classes(["suggested-action", "pill"]).build();
    let banner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["osd", "card"])
        .build();
    banner.set_margin_top(16);
    banner.set_margin_bottom(16);
    banner.set_margin_start(16);
    banner.set_margin_end(16);
    banner.append(&banner_text);
    banner.append(&banner_wake);
    // Scrolls only while zoomed in.
    let screen_scroll = gtk::ScrolledWindow::builder().child(&screen).hexpand(true).vexpand(true).build();
    let screen_box = gtk::Overlay::builder().child(&screen_scroll).build();
    screen_box.add_overlay(&banner);
    let health = gtk::Label::builder()
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .css_classes(["osd", "health"])
        .can_target(false)
        .build();
    health.set_margin_end(12);
    health.set_margin_bottom(12);
    screen_box.add_overlay(&health);
    let css = gtk::CssProvider::new();
    css.load_from_string(
        ".health { padding: 4px 10px; border-radius: 999px; font-feature-settings: 'tnum'; }
         .health.laggy { background: rgba(200, 130, 0, 0.9); color: white; font-weight: bold; }
         .health.stalled { background: rgba(190, 30, 30, 0.92); color: white; font-weight: bold; font-size: 1.3em; }
         .problems { color: #e5a50a; font-weight: bold; }",
    );
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&health),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Right: webcam, toggles, status, cards.
    let webcam = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).can_shrink(true).build();
    // Keeps the webcam 16:9 at whatever width the sidebar is dragged to.
    let webcam_frame = gtk::AspectFrame::builder().ratio(16.0 / 9.0).obey_child(false).child(&webcam).build();
    let toggles = Toggles {
        talk: gtk::ToggleButton::builder().child(&short_label("🎙 Talk")).active(saved.talk).build(),
        hear: gtk::ToggleButton::builder().child(&short_label("🔊 Hear apps")).active(saved.hear).build(),
        cam: gtk::ToggleButton::builder().child(&short_label("📷 My cam")).tooltip_text("Your camera on their screen. Shift+drag on their screen to move it.").build(),
        control: gtk::ToggleButton::builder().child(&short_label("🖱 Control")).build(),
    };
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(6).column_homogeneous(true).build();
    grid.attach(&toggles.talk, 0, 0, 1, 1);
    grid.attach(&toggles.hear, 1, 0, 1, 1);
    grid.attach(&toggles.cam, 0, 1, 1, 1);
    grid.attach(&toggles.control, 1, 1, 1, 1);
    let unlock = gtk::Button::builder().child(&short_label("🔓 Wake & unlock their screen")).build();
    grid.attach(&unlock, 0, 2, 2, 1);
    grid.attach(&stopwatch(), 0, 3, 2, 1);
    let output = device_picker();
    output.set_hexpand(true);
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 5.0);
    volume.set_hexpand(true);
    volume.set_draw_value(true);
    volume.set_value_pos(gtk::PositionType::Right);
    volume.set_format_value_func(|_, v| format!("{v:.0}%"));
    volume.set_sensitive(false);
    let mic = device_picker();
    mic.set_hexpand(true);
    let quality = device_picker();
    quality.set_model(Some(&gtk::StringList::new(&QUALITIES.map(|(label, _)| label))));
    quality.set_hexpand(true);
    let pickers = gtk::Grid::builder().row_spacing(6).column_spacing(8).build();
    // Labels above the pickers, not beside them: keeps the sidebar narrow.
    pickers.attach(&short_label("Their sound on"), 0, 0, 1, 1);
    pickers.attach(&output, 0, 1, 1, 1);
    pickers.attach(&volume, 0, 2, 1, 1);
    pickers.attach(&short_label("Their mic"), 0, 3, 1, 1);
    pickers.attach(&mic, 0, 4, 1, 1);
    pickers.attach(&short_label("Their screen quality"), 0, 5, 1, 1);
    pickers.attach(&quality, 0, 6, 1, 1);
    let shortcuts = shortcuts_button();
    let status = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["dim-label"]).build();
    let side = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(10)
        .margin_bottom(10)
        .margin_end(10)
        .build();
    side.append(&webcam_frame);
    side.append(&grid);
    side.append(&pickers);
    side.append(&shortcuts);
    side.append(&status);

    let mut views = Vec::new();
    for (station, paint) in stations.iter().zip(paints) {
        let thumb = gtk::Picture::builder()
            .paintable(&paint.screen)
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .width_request(72)
            .height_request(54)
            .build();
        let title = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).css_classes(["heading"]).build();
        let problems =
            gtk::Label::builder().xalign(0.0).wrap(true).visible(false).css_classes(["caption", "problems"]).build();
        let sub = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).css_classes(["caption", "dim-label"]).build();
        let level = gtk::LevelBar::for_interval(0.0, 1.0);
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
        text.append(&title);
        text.append(&problems);
        text.append(&sub);
        text.append(&level);
        let row = gtk::Box::builder().spacing(8).build();
        row.append(&thumb);
        row.append(&text);
        let card = gtk::Button::builder().child(&row).build();
        side.append(&card);
        let ss = saved.stations.get(&station.config.name).cloned().unwrap_or_default();
        views.push(View {
            station: station.clone(),
            paint,
            card,
            title,
            problems,
            sub,
            level,
            online: Cell::new(false),
            note: RefCell::new("connecting…".into()),
            dad_cam: Cell::new(ss.dad_cam),
            devices: RefCell::new(None),
            camera_failed: Cell::new(false),
            presence: Cell::new(Presence::default()),
            link: Cell::new(Link::default()),
            quality: Cell::new(ss.quality.min(QUALITIES.len() as u32 - 1)),
            dad_cam_at: Cell::new(ss.dad_cam_at),
        });
    }

    let side_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(140)
        .child(&side)
        .build();
    // Drag the handle to trade sidebar for screen; resizing the window grows the screen.
    let root = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&screen_box)
        .end_child(&side_scroll)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        // Can always be dragged narrower; anything that doesn't fit is clipped.
        .shrink_end_child(true)
        .wide_handle(true)
        .build();
    let placed = Rc::new(Cell::new(false));
    let (pl, sidebar) = (placed.clone(), saved.sidebar.max(0));
    root.connect_max_position_notify(move |p| {
        if !pl.get() && p.max_position() > sidebar * 2 {
            pl.set(true);
            p.set_position(p.max_position() - sidebar);
        }
    });
    let window = gtk::ApplicationWindow::builder().application(app).title("Tutor").default_width(1600).default_height(900).child(&root).build();

    let ui = Rc::new(Ui {
        views,
        me,
        selected: Cell::new(0),
        talk: Cell::new(saved.talk),
        hear: Cell::new(saved.hear),
        paned: root.clone(),
        saved: Rc::new(RefCell::new(saved.clone())),
        pending: Rc::default(),
        control: Cell::new(false),
        screen,
        screen_scroll,
        zoom: Cell::new(1.0),
        pointer: Cell::new((0.0, 0.0)),
        webcam,
        banner,
        banner_text,
        banner_wake,
        health,
        status,
        output,
        volume,
        volume_for: Cell::new(None),
        mic,
        quality,
        toggles,
        updating: Cell::new(false),
        held_keys: RefCell::new(BTreeSet::new()),
        esc_timer: RefCell::new(None),
    });

    for (i, v) in ui.views.iter().enumerate() {
        let u = ui.clone();
        v.card.connect_clicked(move |_| {
            tracing::info!(action = "select", station = %u.views[i].station.config.name, "ui.action");
            u.select(i)
        });
        let (tx, rx) = async_channel::unbounded();
        v.station.start(tx);
        let u = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                u.on_event(i, ev);
            }
        });
    }
    wire_toggles(&ui);
    wire_volume(&ui);
    let u = ui.clone();
    ui.output.connect_selected_notify(move |d| {
        if u.updating.get() {
            return;
        }
        let v = u.view();
        let name = v.devices.borrow().as_ref().and_then(|ds| ds.outputs.get(d.selected() as usize)).map(|(n, _)| n.clone());
        if let Some(name) = name {
            tracing::info!(action = "their_output", station = %v.station.config.name, device = %name, "ui.action");
            v.station.send(ToStation::SetOutput(name));
        }
    });
    let u = ui.clone();
    ui.mic.connect_selected_notify(move |d| {
        if u.updating.get() {
            return;
        }
        let v = u.view();
        let name = v.devices.borrow().as_ref().and_then(|ds| ds.mics.get(d.selected() as usize)).map(|(n, _)| n.clone());
        if let Some(name) = name {
            tracing::info!(action = "their_mic", station = %v.station.config.name, device = %name, "ui.action");
            v.station.send(ToStation::SetMic(name));
        }
    });
    let u = ui.clone();
    ui.quality.connect_selected_notify(move |d| {
        if u.updating.get() {
            return;
        }
        let q = d.selected().min(QUALITIES.len() as u32 - 1);
        tracing::info!(action = "quality", station = %u.view().station.config.name, quality = QUALITIES[q as usize].0, "ui.action");
        u.view().quality.set(q);
        u.sync();
        u.refresh();
    });
    let u = ui.clone();
    unlock.connect_clicked(move |_| wake_unlock(&u));
    let u = ui.clone();
    ui.banner_wake.connect_clicked(move |_| wake_unlock(&u));
    wire_input(&ui);
    wire_view(&ui, &window, &shortcuts);

    let u = ui.clone();
    window.connect_close_request(move |_| {
        tracing::info!("app.closed");
        let now = u.snapshot();
        if *u.saved.borrow() != now {
            settings::save(&now);
        }
        for v in &u.views {
            v.station.stop();
        }
        glib::Propagation::Proceed
    });

    // Only after the first placement, or the initial layout would overwrite the saved width.
    let u = ui.clone();
    root.connect_position_notify(move |_| {
        if placed.get() {
            u.save_soon();
        }
    });
    ui.select(ui.views.iter().position(|v| v.station.config.name == saved.selected).unwrap_or(0));
    window.maximize();
    window.present();
    tracing::info!(stations = ui.views.len(), "app.started");
    Ok(())
}

impl Ui {
    fn view(&self) -> &View {
        &self.views[self.selected.get()]
    }

    fn select(&self, i: usize) {
        self.set_control(false);
        self.selected.set(i);
        let v = self.view();
        self.screen.set_paintable(Some(&v.paint.screen));
        self.webcam.set_paintable(Some(&v.paint.webcam));
        for (j, other) in self.views.iter().enumerate() {
            if j == i {
                other.card.add_css_class("suggested-action");
            } else {
                other.card.remove_css_class("suggested-action");
            }
        }
        self.updating.set(true);
        self.toggles.cam.set_active(v.dad_cam.get());
        self.quality.set_selected(v.quality.get());
        self.updating.set(false);
        // Drops a volume still on its way to the PC we left.
        self.volume_for.set(None);
        self.show_devices();
        self.sync();
        self.refresh();
    }

    /// Settings as the window shows them now.
    fn snapshot(&self) -> Settings {
        Settings {
            selected: self.view().station.config.name.clone(),
            talk: self.talk.get(),
            hear: self.hear.get(),
            sidebar: self.paned.max_position() - self.paned.position(),
            stations: self
                .views
                .iter()
                .map(|v| {
                    let s = StationSettings { quality: v.quality.get(), dad_cam: v.dad_cam.get(), dad_cam_at: v.dad_cam_at.get() };
                    (v.station.config.name.clone(), s)
                })
                .collect(),
        }
    }

    /// Write settings half a second after the last change (a cam drag changes them 60 times a second).
    fn save_soon(&self) {
        let armed = self.pending.borrow_mut().replace(self.snapshot()).is_some();
        if armed {
            return;
        }
        let (pending, saved) = (self.pending.clone(), self.saved.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            let Some(now) = pending.borrow_mut().take() else { return };
            if *saved.borrow() != now {
                settings::save(&now);
                *saved.borrow_mut() = now;
            }
        });
    }

    /// Push every station's wanted streams: every PC sends its mic, the selected one
    /// also its fast screen and webcam (plus app sound and my voice when on), the
    /// others a slow screen.
    fn sync(&self) {
        let sel = self.selected.get();
        let mut any_cam = false;
        for (i, v) in self.views.iter().enumerate() {
            let is_sel = i == sel;
            v.station.set_streams(Streams {
                screen_fps: match QUALITIES[v.quality.get() as usize].1 {
                    0 => 0,
                    _ if is_sel => SELECTED_FPS,
                    _ => BACKGROUND_FPS,
                },
                screen_scale: QUALITIES[v.quality.get() as usize].1.max(1),
                kid_cam: is_sel,
                kid_voice: true,
                app_audio: is_sel && self.hear.get(),
                dad_voice: is_sel && self.talk.get(),
                dad_cam: v.dad_cam.get(),
                dad_cam_at: v.dad_cam_at.get(),
            });
            any_cam |= v.dad_cam.get() && v.online.get();
        }
        if let Err(e) = self.me.set_camera(any_cam) {
            self.status.set_text(&format!("My camera: {e:#}"));
        }
        self.save_soon();
    }

    /// Redraw labels from state.
    fn refresh(&self) {
        let sel = self.selected.get();
        for (i, v) in self.views.iter().enumerate() {
            let dot = if v.online.get() { "🟢" } else { "⚪" };
            v.title.set_text(&format!("{dot} {}", v.station.config.name));
            let mut bits = Vec::new();
            if !v.online.get() {
                bits.push(v.note.borrow().clone());
            } else {
                if i == sel && self.talk.get() {
                    bits.push("talking".to_string());
                }
                if v.dad_cam.get() {
                    bits.push("sees you".to_string());
                }
                if i == sel && self.control.get() {
                    bits.push("you're driving".to_string());
                }
                let link = v.link.get();
                if link.stalled() {
                    bits.insert(0, "⚠ stalled".to_string());
                } else if link.laggy() {
                    bits.insert(0, "⚠ laggy".to_string());
                }
                let p = v.presence.get();
                if p.display_off {
                    bits.insert(0, "💤 screen asleep".to_string());
                } else if p.locked {
                    bits.insert(0, "🔒 locked".to_string());
                } else if p.idle {
                    bits.insert(0, "idle".to_string());
                }
            }
            v.sub.set_text(&bits.join(" · "));
            let problems = if v.online.get() { v.problems() } else { Vec::new() };
            v.problems.set_text(&problems.join(" · "));
            v.problems.set_visible(!problems.is_empty());
        }
        let v = self.view();
        let name = &v.station.config.name;
        let p = v.presence.get();
        if !v.online.get() {
            self.banner.set_visible(true);
            self.banner_wake.set_visible(false);
            self.banner_text.set_text(&format!("{name} is offline\n{}", v.note.borrow()));
            self.status.set_text("");
        } else if QUALITIES[v.quality.get() as usize].1 == 0 {
            self.banner.set_visible(true);
            self.banner_wake.set_visible(false);
            self.banner_text.set_text(&format!(
                "Screen sharing is off\n{name}'s games get every frame. Voice and webcam still work.\nPick a screen quality in the sidebar to see it again."
            ));
            self.status.set_text(&v.note.borrow());
        } else if p.display_off || p.locked {
            let what = if p.display_off { "💤 screen is asleep" } else { "🔒 screen is locked" };
            self.banner.set_visible(true);
            self.banner_wake.set_visible(true);
            self.banner_text.set_text(&format!("Connected, but {name}'s {what}"));
            self.status.set_text(&v.note.borrow());
        } else {
            self.banner.set_visible(false);
            self.status.set_text(&v.note.borrow());
        }
        for t in [&self.toggles.hear, &self.toggles.cam, &self.toggles.control, &self.toggles.talk] {
            t.set_sensitive(v.online.get());
        }
        self.show_health();
    }

    fn on_event(&self, i: usize, ev: Event) {
        let v = &self.views[i];
        match ev {
            Event::Online { hostname, encoder } => {
                v.online.set(true);
                *v.note.borrow_mut() = format!("{hostname} · {encoder}");
                self.sync();
            }
            Event::Offline(reason) => {
                v.presence.set(Presence::default());
                v.online.set(false);
                *v.devices.borrow_mut() = None;
                v.camera_failed.set(false);
                v.level.set_value(0.0);
                *v.note.borrow_mut() = format!("{reason} · retrying");
                if i == self.selected.get() {
                    self.set_control(false);
                }
                self.sync();
            }
            Event::Status(s) => {
                // Cleared by the next frame from their camera (see `Event::Link`).
                if s.starts_with("camera:") {
                    v.camera_failed.set(true);
                }
                *v.note.borrow_mut() = s;
            }
            Event::Presence(p) => v.presence.set(p),
            Event::Devices(d) => {
                *v.devices.borrow_mut() = Some(d);
                if i == self.selected.get() {
                    self.show_devices();
                }
            }
            Event::Level(l) => {
                v.level.set_value(l);
                return;
            }
            Event::Link(link) => {
                let camera_back = link.cam_kbps > 0 && v.camera_failed.replace(false);
                let changed =
                    camera_back || link.laggy() != v.link.get().laggy() || link.stalled() != v.link.get().stalled();
                v.link.set(link);
                if i == self.selected.get() {
                    self.show_health();
                }
                if !changed {
                    return;
                }
            }
        }
        self.refresh();
    }

    /// The health pill for the selected station.
    fn show_health(&self) {
        let v = self.view();
        let h = &self.health;
        h.set_visible(v.online.get());
        let l = v.link.get();
        h.remove_css_class("laggy");
        h.remove_css_class("stalled");
        let text = if l.stalled() {
            h.add_css_class("stalled");
            format!("⚠ Stalled — nothing from them for {:.1} s", l.silent_ms as f64 / 1000.0)
        } else if l.laggy() {
            h.add_css_class("laggy");
            let why = if l.dropped > 0 { format!(" · {} frames dropped", l.dropped) } else { String::new() };
            format!("⚠ Laggy: {} ms{why}", l.rtt_ms)
        } else {
            let mbps = |kbps: u32| kbps as f64 / 1000.0;
            format!(
                "● {} ms · {} fps · screen {:.1} Mb/s · cam {:.1} in, {:.1} out\nmemory: theirs {} MB · tutor {} MB",
                l.rtt_ms,
                l.fps,
                mbps(l.kbps),
                mbps(l.cam_kbps),
                mbps(l.my_cam_kbps),
                l.their_rss_mb,
                l.my_rss_mb
            )
        };
        h.set_text(&text);
    }

    /// Fill the output and mic pickers and the volume from the selected station's devices.
    fn show_devices(&self) {
        let devices = self.view().devices.borrow();
        let empty = Devices::default();
        let d = devices.as_ref().unwrap_or(&empty);
        self.updating.set(true);
        for (picker, list, current) in [(&self.output, &d.outputs, &d.output), (&self.mic, &d.mics, &d.mic)] {
            let names: Vec<&str> = list.iter().map(|(_, desc)| desc.as_str()).collect();
            picker.set_model(Some(&gtk::StringList::new(&names)));
            if let Some(i) = list.iter().position(|(n, _)| n == current) {
                picker.set_selected(i as u32);
            }
            picker.set_sensitive(!list.is_empty());
        }
        if self.volume_for.get().is_none() {
            self.volume.set_value(d.volume as f64);
        }
        self.volume.set_sensitive(!d.output.is_empty());
        self.updating.set(false);
    }

    fn set_control(&self, on: bool) {
        if self.control.get() == on {
            return;
        }
        tracing::info!(action = "control", station = %self.view().station.config.name, on, "ui.action");
        self.control.set(on);
        self.updating.set(true);
        self.toggles.control.set_active(on);
        self.updating.set(false);
        if on {
            self.screen.grab_focus();
            self.screen.set_cursor_from_name(Some("crosshair"));
        } else {
            self.screen.set_cursor(None);
            self.held_keys.borrow_mut().clear();
            if let Some(t) = self.esc_timer.borrow_mut().take() {
                t.remove();
            }
            self.view().station.send(ToStation::Input(Input::ReleaseAll));
        }
        self.refresh();
    }

    /// Zoom the screen view to `z`, keeping the point under `anchor` (view
    /// coordinates; the centre when `None`) where it is.
    fn set_zoom(&self, z: f64, anchor: Option<(f64, f64)>) {
        let z = z.clamp(1.0, 4.0);
        let old = self.zoom.replace(z);
        let (w, h) = (self.screen_scroll.width() as f64, self.screen_scroll.height() as f64);
        if z <= 1.0 {
            self.screen.set_size_request(-1, -1);
        } else {
            self.screen.set_size_request((w * z) as i32, (h * z) as i32);
        }
        let (ax, ay) = anchor.unwrap_or((w / 2.0, h / 2.0));
        let (hadj, vadj) = (self.screen_scroll.hadjustment(), self.screen_scroll.vadjustment());
        let x = (hadj.value() + ax) / old * z - ax;
        let y = (vadj.value() + ay) / old * z - ay;
        // The new size lands at the next layout; set the scroll once it has.
        let frames = Cell::new(0);
        self.screen.add_tick_callback(move |_, _| {
            hadj.set_value(x);
            vadj.set_value(y);
            frames.set(frames.get() + 1);
            if frames.get() < 3 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
    }

    fn input(&self, input: Input) {
        if self.control.get() {
            self.view().station.send(ToStation::Input(input));
        }
    }

    /// Show my pointer dot on their screen under `(x, y)`, in the scrolled view's coordinates.
    fn point_at(&self, x: f64, y: f64) {
        if let Some(p) = self.screen_scroll.compute_point(&self.screen, &graphene::Point::new(x as f32, y as f32))
            && let Some(at) = self.to_abs(p.x() as f64, p.y() as f64)
        {
            self.view().station.send(ToStation::Point(Some(at)));
        }
    }

    /// Widget coordinates on the screen view to the kid's absolute pointer space.
    fn to_abs(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let p = self.screen.paintable()?;
        let (vw, vh) = (p.intrinsic_width() as f64, p.intrinsic_height() as f64);
        let (pw, ph) = (self.screen.width() as f64, self.screen.height() as f64);
        if vw <= 0.0 || vh <= 0.0 || pw <= 0.0 || ph <= 0.0 {
            return None;
        }
        let scale = (pw / vw).min(ph / vh);
        let (dw, dh) = (vw * scale, vh * scale);
        let nx = ((x - (pw - dw) / 2.0) / dw).clamp(0.0, 1.0);
        let ny = ((y - (ph - dh) / 2.0) / dh).clamp(0.0, 1.0);
        let max = ABS_MAX as f64;
        Some(((nx * max).round() as u16, (ny * max).round() as u16))
    }
}

impl View {
    /// What's wrong on their PC until it's fixed, for the card.
    fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if let Some(d) = &*self.devices.borrow() {
            let headset = &self.station.config.headset;
            if d.output.is_empty() {
                out.push("🔇 no speakers");
            } else if !headset.is_empty() && !d.output.contains(headset.as_str()) {
                out.push("🎧 headset unplugged");
            }
            if d.mic.is_empty() {
                out.push("🎙 no mic");
            }
        }
        if self.camera_failed.get() {
            out.push("📷 camera not working");
        }
        out
    }
}

/// Their volume: sent at most every 100 ms while the slider moves, logged once it settles.
fn wire_volume(ui: &Rc<Ui>) {
    let armed = Rc::new(Cell::new(false));
    let settle: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let u = ui.clone();
    ui.volume.connect_value_changed(move |_| {
        if u.updating.get() {
            return;
        }
        u.volume_for.set(Some(u.selected.get()));
        if !armed.replace(true) {
            let (u, armed) = (u.clone(), armed.clone());
            glib::timeout_add_local_once(Duration::from_millis(100), move || {
                armed.set(false);
                if let Some(i) = u.volume_for.get() {
                    u.views[i].station.send(ToStation::SetVolume(u.volume.value().round() as u8));
                }
            });
        }
        if let Some(t) = settle.borrow_mut().take() {
            t.remove();
        }
        let (u, s) = (u.clone(), settle.clone());
        *settle.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(600), move || {
            s.borrow_mut().take();
            if let Some(i) = u.volume_for.take() {
                let percent = u.volume.value().round() as u8;
                tracing::info!(action = "their_volume", station = %u.views[i].station.config.name, percent, "ui.action");
            }
        }));
    });
}

fn wire_toggles(ui: &Rc<Ui>) {
    let u = ui.clone();
    ui.toggles.talk.connect_toggled(move |b| {
        if !u.updating.get() {
            tracing::info!(action = "talk", on = b.is_active(), "ui.action");
            u.talk.set(b.is_active());
            u.sync();
            u.refresh();
        }
    });
    let u = ui.clone();
    ui.toggles.hear.connect_toggled(move |b| {
        if !u.updating.get() {
            tracing::info!(action = "hear_apps", on = b.is_active(), "ui.action");
            u.hear.set(b.is_active());
            u.sync();
        }
    });
    let u = ui.clone();
    ui.toggles.cam.connect_toggled(move |b| {
        if !u.updating.get() {
            tracing::info!(action = "my_cam", station = %u.view().station.config.name, on = b.is_active(), "ui.action");
            u.view().dad_cam.set(b.is_active());
            u.sync();
            u.refresh();
        }
    });
    let u = ui.clone();
    ui.toggles.control.connect_toggled(move |b| {
        if !u.updating.get() {
            u.set_control(b.is_active());
        }
    });
}

fn wire_input(ui: &Rc<Ui>) {
    let motion = gtk::EventControllerMotion::new();
    let u = ui.clone();
    motion.connect_motion(move |_, x, y| {
        u.pointer.set((x, y));
        if u.control.get()
            && let Some((x, y)) = u.to_abs(x, y)
        {
            u.input(Input::Move { x, y });
        }
    });
    ui.screen.add_controller(motion);

    // Raw press/release, so drags never lose a release the way gestures can.
    let buttons = gtk::EventControllerLegacy::new();
    let u = ui.clone();
    buttons.connect_event(move |_, event| {
        if !u.control.get() {
            return glib::Propagation::Proceed;
        }
        let down = match event.event_type() {
            gdk::EventType::ButtonPress => true,
            gdk::EventType::ButtonRelease => false,
            _ => return glib::Propagation::Proceed,
        };
        if let Some(b) = event.downcast_ref::<gdk::ButtonEvent>() {
            if down {
                u.screen.grab_focus();
            }
            u.input(Input::Button { button: b.button() as u8, down });
        }
        glib::Propagation::Stop
    });
    ui.screen.add_controller(buttons);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let u = ui.clone();
    scroll.connect_scroll(move |c, dx, dy| {
        if c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
            let (px, py) = u.pointer.get();
            let anchor = (px - u.screen_scroll.hadjustment().value(), py - u.screen_scroll.vadjustment().value());
            u.set_zoom(u.zoom.get() * if dy < 0.0 { 1.25 } else { 0.8 }, Some(anchor));
            return glib::Propagation::Stop;
        }
        if !u.control.get() {
            return glib::Propagation::Proceed;
        }
        // A wheel detent is 1.0; touchpads report pixels.
        let k = if c.unit() == gdk::ScrollUnit::Wheel { 120.0 } else { 12.0 };
        u.input(Input::Scroll { dx: (dx * k) as i32, dy: (dy * k) as i32 });
        glib::Propagation::Stop
    });
    ui.screen.add_controller(scroll);

    let keys = gtk::EventControllerKey::new();
    let u = ui.clone();
    keys.connect_key_pressed(move |_, _, keycode, _| {
        if !u.control.get() || keycode < 8 {
            return glib::Propagation::Proceed;
        }
        let code = (keycode - 8) as u16;
        // Their PC does its own key repeat.
        if u.held_keys.borrow_mut().insert(code) {
            u.input(Input::Key { code, down: true });
            if keycode == ESC_KEYCODE {
                let w = Rc::downgrade(&u);
                let t = glib::timeout_add_local_once(Duration::from_secs(1), move || {
                    if let Some(u) = w.upgrade() {
                        u.esc_timer.borrow_mut().take();
                        u.set_control(false);
                    }
                });
                u.esc_timer.replace(Some(t));
            }
        }
        glib::Propagation::Stop
    });
    let u = ui.clone();
    keys.connect_key_released(move |_, _, keycode, _| {
        if !u.control.get() || keycode < 8 {
            return;
        }
        let code = (keycode - 8) as u16;
        if keycode == ESC_KEYCODE
            && let Some(t) = u.esc_timer.borrow_mut().take()
        {
            t.remove();
        }
        if u.held_keys.borrow_mut().remove(&code) {
            u.input(Input::Key { code, down: false });
        }
    });
    ui.screen.add_controller(keys);

    // Alt-tabbing away mid-keypress must not leave keys stuck down on their PC.
    let focus = gtk::EventControllerFocus::new();
    let u = ui.clone();
    focus.connect_leave(move |_| {
        if u.control.get() {
            u.held_keys.borrow_mut().clear();
            u.input(Input::ReleaseAll);
        }
    });
    ui.screen.add_controller(focus);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Pan,
    MoveCam,
    Point,
}

/// Pointing, zoom, pan and full screen. All of it only while Control is off, when the
/// mouse and keys are ours rather than theirs (Ctrl+scroll zooms either way).
fn wire_view(ui: &Rc<Ui>, window: &gtk::ApplicationWindow, shortcuts: &gtk::MenuButton) {
    // Left drag points at their screen (Shift: moves my cam there); any other button pans while zoomed.
    let drag = gtk::GestureDrag::builder().button(0).build();
    let start = Rc::new(Cell::new((0.0, 0.0)));
    let mode = Rc::new(Cell::new(Drag::Pan));
    let (u, st, md) = (ui.clone(), start.clone(), mode.clone());
    drag.connect_drag_begin(move |g, x, y| {
        let shift = g.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
        md.set(match g.current_button() {
            _ if u.control.get() => Drag::Pan,
            1 if shift => Drag::MoveCam,
            1 => Drag::Point,
            _ => Drag::Pan,
        });
        if md.get() == Drag::Point {
            tracing::info!(action = "point", station = %u.view().station.config.name, "ui.action");
            u.point_at(x, y);
        }
        st.set((u.screen_scroll.hadjustment().value(), u.screen_scroll.vadjustment().value()));
    });
    let (u, md) = (ui.clone(), mode.clone());
    drag.connect_drag_update(move |g, dx, dy| {
        let Some((x0, y0)) = g.start_point() else { return };
        match md.get() {
            Drag::Point => u.point_at(x0 + dx, y0 + dy),
            Drag::MoveCam => {
                let Some(p) =
                    u.screen_scroll.compute_point(&u.screen, &graphene::Point::new((x0 + dx) as f32, (y0 + dy) as f32))
                else {
                    return;
                };
                if let Some(at) = u.to_abs(p.x() as f64, p.y() as f64) {
                    u.view().dad_cam_at.set(at);
                    u.sync();
                }
            }
            Drag::Pan => {
                if u.control.get() || u.zoom.get() <= 1.0 {
                    return;
                }
                let (x, y) = start.get();
                u.screen_scroll.hadjustment().set_value(x - dx);
                u.screen_scroll.vadjustment().set_value(y - dy);
            }
        }
    });
    let u = ui.clone();
    drag.connect_drag_end(move |_, _, _| {
        if mode.replace(Drag::Pan) == Drag::Point {
            u.view().station.send(ToStation::Point(None));
        }
    });
    ui.screen_scroll.add_controller(drag);

    let click = gtk::GestureClick::new();
    let u = ui.clone();
    click.connect_pressed(move |_, n, _, _| {
        if n == 2 && !u.control.get() {
            u.set_zoom(1.0, None);
        }
    });
    ui.screen_scroll.add_controller(click);

    let keys = gtk::EventControllerKey::new();
    let (u, w, shortcuts) = (ui.clone(), window.downgrade(), shortcuts.clone());
    keys.connect_key_pressed(move |_, key, _, state| {
        if u.control.get() {
            return glib::Propagation::Proceed;
        }
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::F11 => {
                if let Some(w) = w.upgrade() {
                    tracing::info!(action = "fullscreen", on = !w.is_fullscreen(), "ui.action");
                    if w.is_fullscreen() { w.unfullscreen() } else { w.fullscreen() }
                }
            }
            gdk::Key::F1 => {
                tracing::info!(action = "shortcuts", "ui.action");
                shortcuts.popup()
            }
            gdk::Key::_0 if ctrl => u.set_zoom(1.0, None),
            gdk::Key::equal | gdk::Key::plus if ctrl => u.set_zoom(u.zoom.get() * 1.25, None),
            gdk::Key::minus if ctrl => u.set_zoom(u.zoom.get() * 0.8, None),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}

/// A label that shortens with "…" instead of holding the sidebar wide.
fn short_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).ellipsize(pango::EllipsizeMode::End).build()
}

/// Click to start timing from zero, click again to stop; the time stays up until the next start.
fn stopwatch() -> gtk::ToggleButton {
    let label = short_label("⏱ Start timer");
    let button = gtk::ToggleButton::builder().child(&label).build();
    let started = Rc::new(Cell::new(std::time::Instant::now()));
    let tick: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    button.connect_toggled(move |b| {
        if let Some(id) = tick.borrow_mut().take() {
            id.remove();
        }
        if b.is_active() {
            tracing::info!(action = "timer_start", "ui.action");
            started.set(std::time::Instant::now());
            label.set_text("⏹ 0:00");
            let (label, started) = (label.clone(), started.clone());
            *tick.borrow_mut() = Some(glib::timeout_add_local(Duration::from_millis(250), move || {
                label.set_text(&format!("⏹ {}", clock(started.get().elapsed())));
                glib::ControlFlow::Continue
            }));
            b.set_tooltip_text(Some("Click to stop"));
        } else {
            tracing::info!(action = "timer_stop", seconds = started.get().elapsed().as_secs(), "ui.action");
            label.set_text(&format!("⏱ Took {} — start again", clock(started.get().elapsed())));
            b.set_tooltip_text(None);
        }
    });
    button
}

/// 4:07, or 1:04:07 past an hour.
fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// A dropdown whose button shortens long device names; the open list shows them whole.
fn device_picker() -> gtk::DropDown {
    let item_factory = |ellipsize: bool| {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let label = gtk::Label::builder().xalign(0.0).build();
            if ellipsize {
                label.set_ellipsize(pango::EllipsizeMode::End);
            }
            item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&label));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            if let (Some(s), Some(label)) =
                (item.item().and_downcast::<gtk::StringObject>(), item.child().and_downcast::<gtk::Label>())
            {
                label.set_text(&s.string());
            }
        });
        factory
    };
    gtk::DropDown::builder()
        .model(&gtk::StringList::new(&[]))
        .factory(&item_factory(true))
        .list_factory(&item_factory(false))
        .hexpand(true)
        .build()
}

/// Every key and mouse trick, one click (or F1) away.
const SHORTCUTS: [(&str, &str); 12] = [
    ("Click / drag", "Point: they see a red dot there"),
    ("Shift + drag", "Move your cam on their screen (My cam on)"),
    ("Ctrl + scroll", "Zoom in/out on their screen (always)"),
    ("Right-drag", "Pan while zoomed"),
    ("Double-click", "Fit their whole screen"),
    ("Ctrl + = / Ctrl + −", "Zoom in / out"),
    ("Ctrl + 0", "Fit their whole screen"),
    ("F11", "Full screen"),
    ("F1", "Show this list"),
    ("Drag the divider", "Make the sidebar narrower or wider"),
    ("🖱 Control", "Your mouse and keys drive their PC"),
    ("Hold Esc 1 s", "Stop Control"),
];

fn shortcuts_button() -> gtk::MenuButton {
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(16)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    for (row, (keys, what)) in SHORTCUTS.iter().enumerate() {
        grid.attach(&gtk::Label::builder().label(*keys).xalign(0.0).css_classes(["heading"]).build(), 0, row as i32, 1, 1);
        grid.attach(&gtk::Label::builder().label(*what).xalign(0.0).build(), 1, row as i32, 1, 1);
    }
    let note = gtk::Label::builder()
        .label("While Control is on, keys go to their PC, so only Ctrl + scroll and Esc work here.")
        .wrap(true)
        .max_width_chars(40)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    grid.attach(&note, 0, SHORTCUTS.len() as i32, 2, 1);
    gtk::MenuButton::builder()
        .child(&short_label("⌨ Shortcuts (F1)"))
        .popover(&gtk::Popover::builder().child(&grid).build())
        .build()
}

/// Wake their display and dismiss the lock screen (logind, no password).
fn wake_unlock(ui: &Rc<Ui>) {
    let station = ui.view().station.clone();
    let name = station.config.name.clone();
    tracing::info!(action = "wake_unlock", station = %name, "ui.action");
    station.send(ToStation::Wake);
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(station.unlock().map_err(|e| format!("{e:#}")));
    });
    let u = ui.clone();
    glib::spawn_future_local(async move {
        let text = match rx.recv().await {
            Ok(Ok(())) => {
                tracing::info!(station = %name, "station.unlocked");
                "woke and unlocked".to_string()
            }
            Ok(Err(e)) => {
                tracing::warn!(station = %name, error = %e, "station.unlock_failed");
                format!("unlock failed: {e}")
            }
            Err(_) => return,
        };
        u.status.set_text(&text);
    });
}
