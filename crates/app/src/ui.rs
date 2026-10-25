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
use gtk4::{self as gtk, gdk, glib, pango, prelude::*};
use tutor_proto::{ABS_MAX, Input, Streams, ToStation};

use crate::{
    config::Config,
    media::{self, Me, Paintables, Playback},
    station::{Devices, Event, Station},
};

/// Starting sidebar width; drag the divider to change it.
const SIDEBAR: i32 = 360;
const SELECTED_FPS: u8 = 30;
const BACKGROUND_FPS: u8 = 5;
/// evdev KEY_ESC, as GDK reports it (X keycode = evdev + 8).
const ESC_KEYCODE: u32 = 9;

struct View {
    station: Arc<Station>,
    paint: Paintables,
    card: gtk::Button,
    title: gtk::Label,
    sub: gtk::Label,
    level: gtk::LevelBar,
    online: Cell<bool>,
    note: RefCell<String>,
    /// "My cam" is remembered per PC.
    dad_cam: Cell<bool>,
    devices: RefCell<Devices>,
}

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
    banner: gtk::Label,
    status: gtk::Label,
    output: gtk::DropDown,
    mic: gtk::DropDown,
    toggles: Toggles,
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
            eprintln!("tutor: {e:#}");
            app.quit();
        }
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &gtk::Application, config: &Config) -> Result<()> {
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
    let banner = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["title-2"]).build();
    // Scrolls only while zoomed in.
    let screen_scroll = gtk::ScrolledWindow::builder().child(&screen).hexpand(true).vexpand(true).build();
    let screen_box = gtk::Overlay::builder().child(&screen_scroll).build();
    screen_box.add_overlay(&banner);

    // Right: webcam, toggles, status, cards.
    let webcam = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).can_shrink(true).build();
    // Keeps the webcam 16:9 at whatever width the sidebar is dragged to.
    let webcam_frame = gtk::AspectFrame::builder().ratio(16.0 / 9.0).obey_child(false).child(&webcam).build();
    let toggles = Toggles {
        talk: gtk::ToggleButton::builder().child(&short_label("🎙 Talk")).active(true).build(),
        hear: gtk::ToggleButton::builder().child(&short_label("🔊 Hear apps")).active(true).build(),
        cam: gtk::ToggleButton::builder().child(&short_label("📷 My cam")).build(),
        control: gtk::ToggleButton::builder().child(&short_label("🖱 Control")).build(),
    };
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(6).column_homogeneous(true).build();
    grid.attach(&toggles.talk, 0, 0, 1, 1);
    grid.attach(&toggles.hear, 1, 0, 1, 1);
    grid.attach(&toggles.cam, 0, 1, 1, 1);
    grid.attach(&toggles.control, 1, 1, 1, 1);
    let unlock = gtk::Button::builder().child(&short_label("🔓 Unlock their screen")).build();
    grid.attach(&unlock, 0, 2, 2, 1);
    let output = device_picker();
    output.set_hexpand(true);
    let mic = device_picker();
    mic.set_hexpand(true);
    let pickers = gtk::Grid::builder().row_spacing(6).column_spacing(8).build();
    // Labels above the pickers, not beside them: keeps the sidebar narrow.
    pickers.attach(&short_label("Their sound on"), 0, 0, 1, 1);
    pickers.attach(&output, 0, 1, 1, 1);
    pickers.attach(&short_label("Their mic"), 0, 2, 1, 1);
    pickers.attach(&mic, 0, 3, 1, 1);
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
        let sub = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).css_classes(["caption", "dim-label"]).build();
        let level = gtk::LevelBar::for_interval(0.0, 1.0);
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
        text.append(&title);
        text.append(&sub);
        text.append(&level);
        let row = gtk::Box::builder().spacing(8).build();
        row.append(&thumb);
        row.append(&text);
        let card = gtk::Button::builder().child(&row).build();
        side.append(&card);
        views.push(View {
            station: station.clone(),
            paint,
            card,
            title,
            sub,
            level,
            online: Cell::new(false),
            note: RefCell::new("connecting…".into()),
            dad_cam: Cell::new(false),
            devices: RefCell::new(Devices::default()),
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
    let placed = Cell::new(false);
    root.connect_max_position_notify(move |p| {
        if !placed.get() && p.max_position() > SIDEBAR * 2 {
            placed.set(true);
            p.set_position(p.max_position() - SIDEBAR);
        }
    });
    let window = gtk::ApplicationWindow::builder().application(app).title("Tutor").default_width(1600).default_height(900).child(&root).build();

    let ui = Rc::new(Ui {
        views,
        me,
        selected: Cell::new(0),
        talk: Cell::new(true),
        hear: Cell::new(true),
        control: Cell::new(false),
        screen,
        screen_scroll,
        zoom: Cell::new(1.0),
        pointer: Cell::new((0.0, 0.0)),
        webcam,
        banner,
        status,
        output,
        mic,
        toggles,
        updating: Cell::new(false),
        held_keys: RefCell::new(BTreeSet::new()),
        esc_timer: RefCell::new(None),
    });

    for (i, v) in ui.views.iter().enumerate() {
        let u = ui.clone();
        v.card.connect_clicked(move |_| u.select(i));
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
    let u = ui.clone();
    ui.output.connect_selected_notify(move |d| {
        if u.updating.get() {
            return;
        }
        let v = u.view();
        let name = v.devices.borrow().outputs.get(d.selected() as usize).map(|(n, _)| n.clone());
        if let Some(name) = name {
            v.station.send(ToStation::SetOutput(name));
        }
    });
    let u = ui.clone();
    ui.mic.connect_selected_notify(move |d| {
        if u.updating.get() {
            return;
        }
        let v = u.view();
        let name = v.devices.borrow().mics.get(d.selected() as usize).map(|(n, _)| n.clone());
        if let Some(name) = name {
            v.station.send(ToStation::SetMic(name));
        }
    });
    let u = ui.clone();
    unlock.connect_clicked(move |_| {
        let station = u.view().station.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(station.unlock().map_err(|e| format!("{e:#}")));
        });
        let u = u.clone();
        glib::spawn_future_local(async move {
            let text = match rx.recv().await {
                Ok(Ok(())) => "unlock sent".to_string(),
                Ok(Err(e)) => format!("unlock failed: {e}"),
                Err(_) => return,
            };
            u.status.set_text(&text);
        });
    });
    wire_input(&ui);
    wire_view(&ui, &window, &shortcuts);

    let u = ui.clone();
    window.connect_close_request(move |_| {
        for v in &u.views {
            v.station.stop();
        }
        glib::Propagation::Proceed
    });

    ui.select(0);
    window.maximize();
    window.present();
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
        self.updating.set(false);
        self.show_devices();
        self.sync();
        self.refresh();
    }

    /// Push every station's wanted streams (see the switching table in the plan).
    fn sync(&self) {
        let sel = self.selected.get();
        let mut any_cam = false;
        for (i, v) in self.views.iter().enumerate() {
            let is_sel = i == sel;
            v.station.set_streams(Streams {
                screen_fps: if is_sel { SELECTED_FPS } else { BACKGROUND_FPS },
                kid_cam: is_sel,
                kid_voice: true,
                app_audio: is_sel && self.hear.get(),
                dad_voice: is_sel && self.talk.get(),
                dad_cam: v.dad_cam.get(),
            });
            any_cam |= v.dad_cam.get() && v.online.get();
        }
        if let Err(e) = self.me.set_camera(any_cam) {
            self.status.set_text(&format!("My camera: {e:#}"));
        }
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
            }
            v.sub.set_text(&bits.join(" · "));
        }
        let v = self.view();
        if v.online.get() {
            self.banner.set_visible(false);
            self.status.set_text(&v.note.borrow());
        } else {
            self.banner.set_visible(true);
            self.banner.set_text(&format!("{} is offline\n{}", v.station.config.name, v.note.borrow()));
            self.status.set_text("");
        }
        for t in [&self.toggles.hear, &self.toggles.cam, &self.toggles.control, &self.toggles.talk] {
            t.set_sensitive(v.online.get());
        }
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
                v.online.set(false);
                v.level.set_value(0.0);
                *v.note.borrow_mut() = format!("{reason} · retrying");
                if i == self.selected.get() {
                    self.set_control(false);
                }
                self.sync();
            }
            Event::Status(s) => *v.note.borrow_mut() = s,
            Event::Devices(d) => {
                *v.devices.borrow_mut() = d;
                if i == self.selected.get() {
                    self.show_devices();
                }
                return;
            }
            Event::Level(l) => {
                v.level.set_value(l);
                return;
            }
        }
        self.refresh();
    }

    /// Fill the output and mic pickers from the selected station's devices.
    fn show_devices(&self) {
        let d = self.view().devices.borrow();
        self.updating.set(true);
        for (picker, list, current) in [(&self.output, &d.outputs, &d.output), (&self.mic, &d.mics, &d.mic)] {
            let names: Vec<&str> = list.iter().map(|(_, desc)| desc.as_str()).collect();
            picker.set_model(Some(&gtk::StringList::new(&names)));
            if let Some(i) = list.iter().position(|(n, _)| n == current) {
                picker.set_selected(i as u32);
            }
            picker.set_sensitive(!list.is_empty());
        }
        self.updating.set(false);
    }

    fn set_control(&self, on: bool) {
        if self.control.get() == on {
            return;
        }
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

fn wire_toggles(ui: &Rc<Ui>) {
    let u = ui.clone();
    ui.toggles.talk.connect_toggled(move |b| {
        if !u.updating.get() {
            u.talk.set(b.is_active());
            u.sync();
            u.refresh();
        }
    });
    let u = ui.clone();
    ui.toggles.hear.connect_toggled(move |b| {
        if !u.updating.get() {
            u.hear.set(b.is_active());
            u.sync();
        }
    });
    let u = ui.clone();
    ui.toggles.cam.connect_toggled(move |b| {
        if !u.updating.get() {
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

/// Zoom, pan and full screen. All of it only while Control is off, when the
/// mouse and keys are ours rather than theirs (Ctrl+scroll zooms either way).
fn wire_view(ui: &Rc<Ui>, window: &gtk::ApplicationWindow, shortcuts: &gtk::MenuButton) {
    let drag = gtk::GestureDrag::new();
    let start = Rc::new(Cell::new((0.0, 0.0)));
    let (u, st) = (ui.clone(), start.clone());
    drag.connect_drag_begin(move |_, _, _| {
        st.set((u.screen_scroll.hadjustment().value(), u.screen_scroll.vadjustment().value()));
    });
    let u = ui.clone();
    drag.connect_drag_update(move |_, dx, dy| {
        if u.control.get() || u.zoom.get() <= 1.0 {
            return;
        }
        let (x, y) = start.get();
        u.screen_scroll.hadjustment().set_value(x - dx);
        u.screen_scroll.vadjustment().set_value(y - dy);
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
                    if w.is_fullscreen() { w.unfullscreen() } else { w.fullscreen() }
                }
            }
            gdk::Key::F1 => shortcuts.popup(),
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
const SHORTCUTS: [(&str, &str); 10] = [
    ("Ctrl + scroll", "Zoom in/out on their screen (always)"),
    ("Drag", "Pan while zoomed"),
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
