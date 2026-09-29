// The compositor side: outputs, the control and screen capture.

use std::env;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread::sleep;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use crate::image::{self, Frame, Layout, Rect};
use crate::keys::Key;
use crate::session;
use crate::protocol::cutefish_computer_control_manager_v1::CutefishComputerControlManagerV1 as ControlManager;
use crate::protocol::cutefish_computer_control_v1::{
    self as control_v1, CutefishComputerControlV1 as Control, Rejection, RevokeReason,
    State as KeyState, WindowState,
};

// How long the user has to answer the permission prompt.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
const PROMPT_CLOSE: Duration = Duration::from_millis(300);
// Real input pauses the agent for two seconds; this waits out a little more.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(6);
const PAUSE_RETRY: Duration = Duration::from_millis(500);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

pub const UNTYPABLE: &str = "The keyboard layout cannot type that, and no text field has the focus.";

pub type Result<T> = std::result::Result<T, String>;

struct Output {
    global: u32,
    output: wl_output::WlOutput,
    xdg: ZxdgOutputV1,
    rect: Rect,
}

#[derive(Clone, Copy, PartialEq)]
enum Grant {
    None,
    Pending,
    Granted,
    // Why it ended, and whether the agent has been told.
    Revoked(Option<RevokeReason>, bool),
}

// An application window, as the compositor lists it.
#[derive(Clone, Debug)]
pub struct Window {
    pub handle: u32,
    pub app_id: String,
    pub title: String,
    // The frame in logical coordinates.
    pub rect: Rect,
    pub active: bool,
    pub minimized: bool,
    pub hidden: bool,
    pub other_desk: bool,
}

#[derive(Default)]
struct Capture {
    width: u32,
    height: u32,
    formats: Vec<wl_shm::Format>,
    transform: Option<wl_output::Transform>,
    done: bool,
    stopped: bool,
    ready: bool,
    failed: bool,
}

struct State {
    xdg_output_manager: ZxdgOutputManagerV1,
    outputs: Vec<Output>,
    control: Option<Control>,
    grant: Grant,
    rejections: Vec<Rejection>,
    capture: Capture,
    windows: Vec<Window>,
    windows_done: bool,
    pointer: Option<(f64, f64)>,
}

impl State {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<State>,
    ) {
        let output = registry.bind::<wl_output::WlOutput, _, _>(global, version.min(4), qh, ());
        let xdg = self.xdg_output_manager.get_xdg_output(&output, qh, global);
        let rect = Rect { x: 0, y: 0, width: 0, height: 0 };
        self.outputs.push(Output { global, output, xdg, rect });
    }
}

fn revoke_message(reason: Option<RevokeReason>) -> String {
    match reason {
        Some(RevokeReason::Denied) => {
            "The user did not allow control of the computer. Do not ask again unless the user asks you to."
        }
        Some(RevokeReason::Stopped) => {
            "The user stopped your control of the computer. Do not continue unless the user asks you to."
        }
        Some(RevokeReason::Locked) => "The session is locked, which ended control of the computer.",
        Some(RevokeReason::Busy) => "Another app is controlling the computer.",
        Some(RevokeReason::Unavailable) => {
            "The user could not be asked for permission to control the computer."
        }
        _ => "Control of the computer ended.",
    }
    .to_string()
}

fn protocol_error(error: impl std::fmt::Display) -> String {
    format!("Lost the connection to the compositor: {error}")
}

fn connect_to_display() -> Result<Connection> {
    if env::var_os("WAYLAND_SOCKET").is_some() || env::var_os("WAYLAND_DISPLAY").is_some() {
        return Connection::connect_to_env()
            .map_err(|e| format!("Cannot connect to the Wayland display: {e}"));
    }
    connect_to_session()
}

fn connect_to_session() -> Result<Connection> {
    let display = session::exported("WAYLAND_DISPLAY")
        .ok_or("Cannot find the Wayland display of the session.")?;
    let stream = UnixStream::connect(session::runtime_dir().join(&display))
        .map_err(|e| format!("Cannot connect to the Wayland display {display}: {e}"))?;
    Connection::from_socket(stream)
        .map_err(|e| format!("Cannot connect to the Wayland display {display}: {e}"))
}

// Requests sent in one go; `pause` lets the compositor and the target
// application see time pass between them.
pub struct Batch<'a> {
    control: &'a Control,
    connection: &'a Connection,
}

impl Batch<'_> {
    pub fn motion(&mut self, (x, y): (f64, f64)) {
        self.control.motion(x, y);
    }

    pub fn button(&mut self, button: u32, pressed: bool) {
        self.control.button(button, key_state(pressed));
    }

    pub fn axis(&mut self, horizontal: i32, vertical: i32) {
        self.control.axis(horizontal, vertical);
    }

    pub fn key(&mut self, key: Key, pressed: bool) {
        match key {
            Key::Code(code) => self.control.key(code, key_state(pressed)),
            Key::Sym(keysym) => self.control.keysym(keysym, key_state(pressed)),
        }
    }

    pub fn type_text(&mut self, text: &str) {
        self.control.type_text(text.to_string());
    }

    pub fn activate_window(&mut self, handle: u32) {
        self.control.activate_window(handle);
    }

    pub fn pause(&mut self, milliseconds: u64) {
        let _ = self.connection.flush();
        sleep(Duration::from_millis(milliseconds));
    }
}

fn key_state(pressed: bool) -> KeyState {
    if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

pub struct Desktop {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    shm: wl_shm::WlShm,
    control_manager: ControlManager,
    sources: ExtOutputImageCaptureSourceManagerV1,
    copy: ExtImageCopyCaptureManagerV1,
    layout: Option<Layout>,
    pub agent_name: String,
}

impl Desktop {
    // Binds everything privileged at once: the compositor identifies this
    // process by its executable only while it is still dumpable.
    pub fn connect() -> Result<Desktop> {
        let connection = connect_to_display()?;
        let (globals, queue) = registry_queue_init::<State>(&connection).map_err(protocol_error)?;
        let qh = queue.handle();
        let control_manager = globals.bind::<ControlManager, _, _>(&qh, 1..=1, ()).map_err(|_| {
            "This session does not let cutefish-computer-use control the computer.".to_string()
        })?;
        let missing = |name: &str| format!("The compositor has no {name}.");
        let shm = globals.bind(&qh, 1..=1, ()).map_err(|_| missing("wl_shm"))?;
        let sources = globals.bind(&qh, 1..=1, ()).map_err(|_| missing("screen capture"))?;
        let copy = globals.bind(&qh, 1..=1, ()).map_err(|_| missing("screen capture"))?;
        let xdg_output_manager =
            globals.bind(&qh, 2..=3, ()).map_err(|_| missing("xdg_output"))?;

        let mut state = State {
            xdg_output_manager,
            outputs: Vec::new(),
            control: None,
            grant: Grant::None,
            rejections: Vec::new(),
            capture: Capture::default(),
            windows: Vec::new(),
            windows_done: false,
            pointer: None,
        };
        let registry = globals.registry().clone();
        globals.contents().with_list(|list| {
            for global in list.iter().filter(|g| g.interface == "wl_output") {
                state.add_output(&registry, global.name, global.version, &qh);
            }
        });
        let mut desktop = Desktop {
            connection,
            queue,
            qh,
            state,
            shm,
            control_manager,
            sources,
            copy,
            layout: None,
            agent_name: String::new(),
        };
        desktop.queue.roundtrip(&mut desktop.state).map_err(protocol_error)?;
        Ok(desktop)
    }

    // Dispatches events until `done` holds or `deadline` passes.
    fn dispatch_until(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> Result<bool> {
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(protocol_error)?;
            if done(&self.state) {
                return Ok(true);
            }
            self.queue.flush().map_err(protocol_error)?;
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut poll = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
            if ready > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(protocol_error(e)),
                }
            }
        }
    }

    // Takes in whatever the compositor sent meanwhile, such as a revocation.
    fn pump(&mut self) -> Result<()> {
        self.dispatch_until(Instant::now(), |_| false).map(|_| ())
    }

    fn drop_control(&mut self) {
        if let Some(control) = self.state.control.take() {
            control.destroy();
        }
        let _ = self.connection.flush();
    }

    // A granted control, asking the user for one if need be. A control the
    // user ended is reported once before the next call asks again.
    fn ensure_control(&mut self) -> Result<Control> {
        self.pump()?;
        match self.state.grant {
            Grant::Granted => {
                if let Some(control) = &self.state.control {
                    return Ok(control.clone());
                }
            }
            Grant::Revoked(reason, false) => {
                self.state.grant = Grant::Revoked(reason, true);
                self.drop_control();
                return Err(revoke_message(reason));
            }
            _ => {}
        }
        self.drop_control();
        let control = self.control_manager.request_control(self.agent_name.clone(), &self.qh, ());
        self.state.control = Some(control.clone());
        self.state.grant = Grant::Pending;
        self.dispatch_until(Instant::now() + CONSENT_TIMEOUT, |s| s.grant != Grant::Pending)?;
        match self.state.grant {
            Grant::Granted => {
                // The prompt takes a moment to animate away; a screenshot right
                // away would still show it.
                sleep(PROMPT_CLOSE);
                Ok(control)
            }
            Grant::Revoked(reason, _) => {
                self.state.grant = Grant::Revoked(reason, true);
                self.drop_control();
                Err(revoke_message(reason))
            }
            _ => {
                // Destroying the control withdraws the prompt.
                self.drop_control();
                self.state.grant = Grant::None;
                Err("The user did not answer the permission prompt in time.".into())
            }
        }
    }

    // Sends `batch` and waits for the compositor to handle it. A batch must
    // release whatever it presses: a paused one is sent again from the start.
    pub fn inject(&mut self, batch: &dyn Fn(&mut Batch)) -> Result<()> {
        if self.try_inject(batch)? {
            Ok(())
        } else {
            Err(UNTYPABLE.into())
        }
    }

    // As inject, but an untypable batch is Ok(false) rather than an error.
    pub fn try_inject(&mut self, batch: &dyn Fn(&mut Batch)) -> Result<bool> {
        let deadline = Instant::now() + PAUSE_TIMEOUT;
        loop {
            let control = self.ensure_control()?;
            self.state.rejections.clear();
            batch(&mut Batch { control: &control, connection: &self.connection });
            self.queue.roundtrip(&mut self.state).map_err(protocol_error)?;
            if let Grant::Revoked(reason, false) = self.state.grant {
                self.state.grant = Grant::Revoked(reason, true);
                self.drop_control();
                return Err(revoke_message(reason));
            }
            let rejections = std::mem::take(&mut self.state.rejections);
            if rejections.is_empty() {
                return Ok(true);
            }
            if rejections.contains(&Rejection::Protected) {
                return Err("That is a system dialog, which only the user may answer. Ask the user to do it.".into());
            }
            if rejections.contains(&Rejection::Untypable) {
                return Ok(false);
            }
            if rejections.contains(&Rejection::NoWindow) {
                return Err("That window is gone. List the windows again.".into());
            }
            if rejections.contains(&Rejection::Paused) {
                if Instant::now() + PAUSE_RETRY < deadline {
                    sleep(PAUSE_RETRY);
                    continue;
                }
                return Err("The user is using the pointer or keyboard, so your input is paused. Wait and try again.".into());
            }
            return Err("The compositor did not take the input.".into());
        }
    }

    // Sends a query and waits for its answer: the control must be granted.
    fn query(&mut self, send: impl Fn(&Control)) -> Result<()> {
        let control = self.ensure_control()?;
        self.state.rejections.clear();
        send(&control);
        self.queue.roundtrip(&mut self.state).map_err(protocol_error)?;
        if let Grant::Revoked(reason, false) = self.state.grant {
            self.state.grant = Grant::Revoked(reason, true);
            self.drop_control();
            return Err(revoke_message(reason));
        }
        if !std::mem::take(&mut self.state.rejections).is_empty() {
            return Err("The compositor did not answer.".into());
        }
        Ok(())
    }

    fn output_rects(&self) -> Vec<Rect> {
        self.state.outputs.iter().map(|o| o.rect).collect()
    }

    // The latest screenshot's layout, which every x and y refers to.
    pub fn layout(&mut self) -> Result<Layout> {
        self.pump()?;
        match self.layout {
            Some(layout) => Ok(layout),
            None => Layout::of(&self.output_rects()).ok_or_else(|| "There is no display.".into()),
        }
    }

    // Screenshot pixels to logical coordinates, by the latest screenshot.
    pub fn to_logical(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        Ok(self.layout()?.to_logical(x, y))
    }

    // Application windows, most recently used first.
    pub fn windows(&mut self) -> Result<Vec<Window>> {
        self.state.windows.clear();
        self.state.windows_done = false;
        self.query(|control| control.list_windows())?;
        if !self.state.windows_done {
            return Err("The compositor did not list the windows.".into());
        }
        Ok(std::mem::take(&mut self.state.windows))
    }

    // Where the pointer is, in logical coordinates.
    pub fn pointer(&mut self) -> Result<(f64, f64)> {
        self.state.pointer = None;
        self.query(|control| control.get_pointer())?;
        self.state.pointer.ok_or_else(|| "The compositor did not tell where the pointer is.".into())
    }

    pub fn screenshot(&mut self) -> Result<(Vec<u8>, &'static str, Layout)> {
        self.ensure_control()?;
        let layout = Layout::of(&self.output_rects()).ok_or("There is no display.")?;
        let frames = self.capture_all()?;
        let (data, mime_type) = render(&layout, &frames)?;
        self.layout = Some(layout);
        Ok((data, mime_type, layout))
    }

    // Part of the latest screenshot, at up to the displays' own resolution.
    // The latest screenshot stays what coordinates refer to.
    pub fn zoom(&mut self, x: f64, y: f64, width: f64, height: f64) -> Result<(Vec<u8>, &'static str, Layout)> {
        let region = self
            .layout()?
            .to_logical_rect(x, y, width, height)
            .ok_or("That region is outside the screenshot.")?;
        self.ensure_control()?;
        let frames = self.capture_all()?;
        let native = frames
            .iter()
            .filter(|(rect, _)| rect.intersection(&region).is_some())
            .map(|(rect, frame)| frame.width as f64 / rect.width as f64)
            .fold(1.0, f64::max);
        let layout = Layout::fit(region, native);
        let (data, mime_type) = render(&layout, &frames)?;
        Ok((data, mime_type, layout))
    }

    fn capture_all(&mut self) -> Result<Vec<(Rect, Frame)>> {
        let outputs: Vec<(wl_output::WlOutput, Rect)> =
            self.state.outputs.iter().map(|o| (o.output.clone(), o.rect)).collect();
        outputs.into_iter().map(|(output, rect)| Ok((rect, self.capture(&output)?))).collect()
    }

    fn capture(&mut self, output: &wl_output::WlOutput) -> Result<Frame> {
        let source = self.sources.create_source(output, &self.qh, ());
        let session = self.copy.create_session(&source, Options::PaintCursors, &self.qh, ());
        self.state.capture = Capture::default();
        let result = self.capture_session(&session);
        session.destroy();
        source.destroy();
        let _ = self.connection.flush();
        result
    }

    fn capture_session(&mut self, session: &ExtImageCopyCaptureSessionV1) -> Result<Frame> {
        let failed = || "The screen could not be captured.".to_string();
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        self.dispatch_until(deadline, |s| s.capture.done || s.capture.stopped)?;
        let capture = &self.state.capture;
        if !capture.done || capture.stopped || capture.width == 0 || capture.height == 0 {
            return Err(failed());
        }
        let format = [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
            .into_iter()
            .find(|f| capture.formats.contains(f))
            .ok_or_else(failed)?;
        let (width, height) = (capture.width as usize, capture.height as usize);
        let stride = width * 4;
        let size = stride * height;

        // SAFETY: memfd_create takes a NUL-terminated name and returns a new fd.
        let fd = unsafe { libc::memfd_create(c"cutefish-computer-use".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(failed());
        }
        // SAFETY: the fd was just created and is owned here alone.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.set_len(size as u64).map_err(|_| failed())?;
        // SAFETY: the memfd is private to this process and the compositor,
        // which writes it only between capture and ready.
        let memory = unsafe { memmap2::Mmap::map(&file) }.map_err(|_| failed())?;
        let pool = self.shm.create_pool(file.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.qh,
            (),
        );
        let frame = session.create_frame(&self.qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        let finished = self.dispatch_until(deadline, |s| {
            s.capture.ready || s.capture.failed || s.capture.stopped
        });
        frame.destroy();
        buffer.destroy();
        pool.destroy();
        finished?;
        let capture = &self.state.capture;
        if !capture.ready {
            return Err(failed());
        }
        if capture.transform.is_some_and(|t| t != wl_output::Transform::Normal) {
            return Err("Rotated displays cannot be captured yet.".into());
        }
        Ok(Frame { width, height, stride, data: memory.to_vec() })
    }
}

fn render(layout: &Layout, frames: &[(Rect, Frame)]) -> Result<(Vec<u8>, &'static str)> {
    let mut canvas = vec![0u8; layout.width as usize * layout.height as usize * 3];
    for (rect, frame) in frames {
        image::draw(&mut canvas, layout, rect, frame);
    }
    image::encode(&canvas, layout.width, layout.height)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                state.add_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(index) = state.outputs.iter().position(|o| o.global == name) {
                    let output = state.outputs.remove(index);
                    output.xdg.destroy();
                    if output.output.version() >= 3 {
                        output.output.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.rect.x = x;
                output.rect.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.rect.width = width;
                output.rect.height = height;
            }
            _ => {}
        }
    }
}

impl Dispatch<Control, ()> for State {
    fn event(
        state: &mut Self,
        control: &Control,
        event: control_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Events of a control already given up on are stale.
        if state.control.as_ref() != Some(control) {
            return;
        }
        match event {
            control_v1::Event::Granted => state.grant = Grant::Granted,
            control_v1::Event::Revoked { reason } => {
                state.grant = Grant::Revoked(reason.into_result().ok(), false);
            }
            control_v1::Event::Rejected { reason } => {
                if let WEnum::Value(reason) = reason {
                    state.rejections.push(reason);
                }
            }
            control_v1::Event::Window {
                handle,
                app_id,
                title,
                x,
                y,
                width,
                height,
                state: flags,
            } => {
                let flags = flags.into_result().unwrap_or(WindowState::empty());
                state.windows.push(Window {
                    handle,
                    app_id,
                    title,
                    rect: Rect { x, y, width, height },
                    active: flags.contains(WindowState::Active),
                    minimized: flags.contains(WindowState::Minimized),
                    hidden: flags.contains(WindowState::Hidden),
                    other_desk: flags.contains(WindowState::OtherDesk),
                });
            }
            control_v1::Event::WindowsDone => state.windows_done = true,
            control_v1::Event::Pointer { x, y } => state.pointer = Some((x, y)),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.capture.width = width;
                state.capture.height = height;
            }
            Event::ShmFormat { format: WEnum::Value(format) } => state.capture.formats.push(format),
            Event::Done => state.capture.done = true,
            Event::Stopped => state.capture.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform { transform: WEnum::Value(transform) } => {
                state.capture.transform = Some(transform);
            }
            Event::Ready => state.capture.ready = true,
            Event::Failed { .. } => state.capture.failed = true,
            _ => {}
        }
    }
}

// Objects whose events carry nothing this needs.
macro_rules! ignore_events {
    ($($proxy:ty),* $(,)?) => {
        $(
            impl Dispatch<$proxy, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$proxy,
                    _: <$proxy as wayland_client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore_events!(
    wl_output::WlOutput,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ZxdgOutputManagerV1,
    ControlManager,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
);
