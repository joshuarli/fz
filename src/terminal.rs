// Terminal behavior derived from fzy, copyright (c) 2014 John Hawthorn.
// SPDX-License-Identifier: MIT. See LICENSE.

//! The small POSIX boundary used by the interactive selector.
//!
//! The editor deliberately stores the query as bytes because fzy matches bytes.
//! Cursor movement and deletion only step at UTF-8 code point boundaries, so a
//! terminal user never lands in the middle of a normally encoded character.

#[cfg(target_os = "linux")]
use rustix::{
    event::{self, PollFd, PollFlags, Timespec},
    io::{self as rustix_io, Errno},
};
#[cfg(target_os = "macos")]
use rustix::{
    event::kqueue::{self, Event, EventFilter, EventFlags},
    process::Signal,
};
use rustix::termios::{self, InputModes, LocalModes, OptionalActions, Termios};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
#[cfg(target_os = "linux")]
use std::marker::PhantomData;
use std::mem;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::mem::MaybeUninit;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::rc::Rc;
use std::time::Duration;

#[cfg(not(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("fz's terminal boundary supports Linux and macOS on x86_64 or aarch64");

#[cfg(target_os = "macos")]
struct ResizeEvents {
    // kqueue keeps the raw input descriptor registered, so this field must
    // precede `Terminal::input` and be dropped first.
    queue: OwnedFd,
}

#[cfg(target_os = "linux")]
struct ResizeEvents {
    signal_fd: OwnedFd,
    // `pthread_sigmask` is thread-local. The thread that installs this
    // descriptor must also restore this exact complete mask.
    original_mask: Option<libc::sigset_t>,
    // `sigset_t` itself is Send, but moving it would restore one thread's
    // mask on another thread. Keep the whole Terminal on its opening thread.
    _thread_bound: PhantomData<Rc<()>>,
}

#[cfg(target_os = "linux")]
impl ResizeEvents {
    fn install(_: &File) -> io::Result<Self> {
        let winch_mask = winch_mask()?;
        // Some libc ABIs write only the kernel-sized prefix of sigset_t. Its
        // all-zero representation is a valid empty mask, so pre-zero unused
        // ABI storage before Rust later treats the full value as initialized.
        let mut original_mask = MaybeUninit::<libc::sigset_t>::zeroed();
        // SAFETY: `winch_mask` is initialized and `original_mask` is writable.
        // pthread_sigmask returns its error number directly rather than errno.
        let mask_result = unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &winch_mask, original_mask.as_mut_ptr())
        };
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }
        // SAFETY: pthread_sigmask initialized the kernel-sized prefix; the
        // zeroed remainder was already initialized as valid mask storage.
        let original_mask = unsafe { original_mask.assume_init() };

        // SIGWINCH remains blocked for this terminal's lifetime. A resize
        // arriving while this descriptor is created or while poll starts is
        // pending, so it makes this descriptor readable instead of racing a
        // handler's check-before-poll window.
        // SAFETY: -1 requests a new descriptor, `winch_mask` is initialized,
        // and these are valid Linux signalfd flags.
        let raw_fd = unsafe {
            libc::signalfd(
                -1,
                &winch_mask,
                libc::SFD_CLOEXEC | libc::SFD_NONBLOCK,
            )
        };
        if raw_fd == -1 {
            let signal_error = io::Error::last_os_error();
            return match restore_signal_mask(&original_mask) {
                Ok(()) => Err(signal_error),
                Err(restore_error) => Err(restore_error),
            };
        }

        // SAFETY: signalfd returned a newly owned nonnegative descriptor.
        let signal_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        Ok(Self {
            signal_fd,
            original_mask: Some(original_mask),
            _thread_bound: PhantomData,
        })
    }

    fn wait_for_input(&self, input: &File, timeout: Option<Duration>) -> io::Result<Wait> {
        let timeout = timeout.map(|timeout| {
            Timespec::try_from(timeout).expect("the fixed escape timeout fits in a timespec")
        });
        let mut descriptors = [
            PollFd::new(input, PollFlags::IN),
            PollFd::new(&self.signal_fd, PollFlags::IN),
        ];
        match event::poll(&mut descriptors, timeout.as_ref()) {
            Ok(0) => Ok(Wait::TimedOut),
            Ok(_) if descriptors[1].revents().contains(PollFlags::IN) => {
                self.drain_signals()?;
                // Input may be ready too. Redraw first; it stays readable for
                // the next iteration, which preserves the resize transition.
                Ok(Wait::Interrupted)
            }
            Ok(_) if descriptors[1].revents().is_empty() => Ok(Wait::Ready),
            Ok(_) => Err(io::Error::other("signalfd stopped reporting SIGWINCH")),
            // Other signals can still interrupt poll. Redraw and retry, as
            // the existing terminal loop did for an interrupted wait.
            Err(Errno::INTR) => Ok(Wait::Interrupted),
            Err(error) => Err(error.into()),
        }
    }

    fn drain_signals(&self) -> io::Result<()> {
        // signalfd requires a buffer at least as large as this ABI-owned
        // record. Its payload is intentionally unused: this descriptor only
        // accepts SIGWINCH, and every queued record means redraw.
        let mut record = [0_u8; mem::size_of::<libc::signalfd_siginfo>()];
        loop {
            match rustix_io::read(&self.signal_fd, &mut record) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "signalfd reached EOF",
                    ));
                }
                Ok(bytes) if bytes == record.len() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "signalfd returned a partial signal record",
                    ));
                }
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let Some(original_mask) = self.original_mask.take() else {
            return Ok(());
        };
        if let Err(error) = restore_signal_mask(&original_mask) {
            // Retain it so Terminal::drop can retry after an explicit restore
            // failure. The descriptor stays open until this field drops.
            self.original_mask = Some(original_mask);
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for ResizeEvents {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(target_os = "linux")]
fn winch_mask() -> io::Result<libc::sigset_t> {
    // See install: a zero mask is valid and initializes ABI storage libc may
    // not touch outside the kernel-sized signal-set prefix.
    let mut mask = MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: the libc functions initialize the supplied writable sigset_t,
    // and SIGWINCH is a valid Linux signal number.
    if unsafe { libc::sigemptyset(mask.as_mut_ptr()) } == -1
        || unsafe { libc::sigaddset(mask.as_mut_ptr(), libc::SIGWINCH) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zeroed storage was already initialized, and the libc calls set
    // the mask's required kernel-visible bits.
    Ok(unsafe { mask.assume_init() })
}

#[cfg(target_os = "linux")]
fn restore_signal_mask(mask: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: `mask` was captured by pthread_sigmask for this same thread and
    // restores its complete caller-owned signal mask.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

#[cfg(target_os = "macos")]
impl ResizeEvents {
    fn install(input: &File) -> io::Result<Self> {
        let queue = kqueue::kqueue()?;
        let receipt_flags = EventFlags::ADD | EventFlags::RECEIPT;
        let changes = [
            Event::new(
                EventFilter::Read(input.as_raw_fd()),
                receipt_flags,
                std::ptr::null_mut(),
            ),
            Event::new(
                EventFilter::Signal {
                    signal: Signal::WINCH,
                    times: 0,
                },
                receipt_flags | EventFlags::CLEAR,
                std::ptr::null_mut(),
            ),
        ];
        let mut receipts = [MaybeUninit::uninit(); 2];
        // SAFETY: `Terminal` owns `input` for longer than this queue; its
        // field order drops the queue before the descriptor it registered.
        let (receipts, _) = unsafe { kqueue::kevent(&queue, &changes, &mut receipts, None)? };
        for receipt in receipts {
            if receipt.flags().contains(EventFlags::ERROR) && receipt.data() != 0 {
                return Err(io::Error::from_raw_os_error(receipt.data() as i32));
            }
        }
        Ok(Self { queue })
    }

    fn wait_for_input(&self, input: &File, timeout: Option<Duration>) -> io::Result<Wait> {
        let mut events = [MaybeUninit::uninit(); 2];
        // SAFETY: `input` remains valid for the queue's full lifetime; see
        // `install` and the `Terminal` field-order invariant above.
        let (events, _) = unsafe { kqueue::kevent(&self.queue, &[], &mut events, timeout)? };
        for event in events.iter() {
            if event.flags().contains(EventFlags::ERROR) && event.data() != 0 {
                return Err(io::Error::from_raw_os_error(event.data() as i32));
            }
        }
        if events.iter().any(|event| {
            matches!(
                event.filter(),
                EventFilter::Signal { signal, .. } if signal == Signal::WINCH
            )
        }) {
            return Ok(Wait::Interrupted);
        }
        if events.iter().any(|event| {
            matches!(
                event.filter(),
                EventFilter::Read(fd) if fd == input.as_raw_fd()
            )
        }) {
            return Ok(Wait::Ready);
        }
        if events.is_empty() {
            Ok(Wait::TimedOut)
        } else {
            Ok(Wait::Interrupted)
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        // EVFILT_SIGNAL observes SIGWINCH without changing its disposition.
        Ok(())
    }
}

/// A raw-mode terminal. Dropping it always puts its saved termios state back.
pub struct Terminal {
    // On macOS, the queue references `input` by raw descriptor. Rust drops
    // fields in declaration order, so keep the queue first.
    resize_events: ResizeEvents,
    input: File,
    output: BufWriter<File>,
    original: Termios,
    restore_needed: bool,
    foreground: u8,
}

impl Terminal {
    /// Opens `path` for input and output, then switches input to fzy's raw mode.
    pub fn open(path: &Path) -> io::Result<Self> {
        let input = OpenOptions::new().read(true).open(path)?;
        let output = OpenOptions::new().write(true).open(path)?;

        let original = termios::tcgetattr(&input)?;
        let mut raw = original.clone();
        raw.input_modes.remove(InputModes::ICRNL);
        raw.local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG);
        termios::tcsetattr(&input, OptionalActions::Now, &raw)?;

        let resize_events = match ResizeEvents::install(&input) {
            Ok(resize_events) => resize_events,
            Err(error) => {
                let _ = termios::tcsetattr(&input, OptionalActions::Now, &original);
                return Err(error);
            }
        };

        let mut terminal = Self {
            resize_events,
            input,
            output: BufWriter::with_capacity(16 * 1024, output),
            original,
            restore_needed: true,
            foreground: 9,
        };
        terminal.set_normal()?;
        terminal.flush()?;
        Ok(terminal)
    }

    /// Runs the interactive selector and returns a chosen candidate or query.
    ///
    /// `None` represents an explicit cancellation. Returned bytes do not have a
    /// trailing newline; the caller owns stdout and adds the record delimiter.
    pub fn run(
        &mut self,
        candidates: fz::Candidates,
        options: &crate::Options,
    ) -> io::Result<Option<Vec<u8>>> {
        if !self.restore_needed {
            return Err(io::Error::other("terminal has already been restored"));
        }

        let (_, height) = self.dimensions();
        let reserved_rows = 1usize + usize::from(options.show_info);
        let lines = options
            .lines
            .min(candidates.len())
            .min(height.saturating_sub(reserved_rows));
        let mut state = Editor::new(options.query.clone(), lines);
        let mut choices = fz::Choices::from_candidates(candidates);
        choices.set_workers(options.workers);
        // On Linux, `Terminal::open` already blocked SIGWINCH. Any search
        // workers spawned here inherit that mask, keeping resize delivery on
        // this thread's signalfd for the whole interactive session.
        choices.search(&state.query);

        self.draw(&choices, options, &state)?;
        let mut input_ready = false;
        loop {
            // Only a bare Escape is ambiguous. Other partial key sequences
            // wait for their next byte, exactly like fzy's keybinding parser.
            let timeout = state
                .ambiguous_key_pending
                .then_some(Duration::from_millis(25));
            let ready = if input_ready { Wait::Ready } else { self.wait_for_input(timeout)? };
            input_ready = false;
            let byte = match ready {
                Wait::Ready => Some(self.read_byte()?.ok_or(io::ErrorKind::UnexpectedEof)?),
                Wait::Interrupted => {
                    self.draw(&choices, options, &state)?;
                    continue;
                }
                Wait::TimedOut => None,
            };
            match self.handle_input(&mut state, &mut choices, byte, byte.is_none()) {
                InputResult::Continue { search_changed } => {
                    state.search_dirty |= search_changed;
                }
                InputResult::Accept => return self.finish(&mut state, &choices, options.show_info),
                InputResult::Cancel => return self.cancel(&state, options.show_info),
            }
            // Drain already queued typing before ranking and drawing. fzy also
            // searches once per input batch; actions that consume the selection
            // synchronize immediately in apply_action instead.
            if matches!(self.wait_for_input(Some(Duration::ZERO))?, Wait::Ready) {
                input_ready = true;
                continue;
            }
            state.update_choices(&mut choices);
            self.draw(&choices, options, &state)?;
        }
    }

    fn finish(
        &mut self,
        state: &mut Editor,
        choices: &fz::Choices,
        show_info: bool,
    ) -> io::Result<Option<Vec<u8>>> {
        let selection = choices
            .selected()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| mem::take(&mut state.query));
        self.clear(state.lines, show_info)?;
        self.restore()?;
        Ok(Some(selection))
    }

    fn cancel(&mut self, state: &Editor, show_info: bool) -> io::Result<Option<Vec<u8>>> {
        self.clear(state.lines, show_info)?;
        self.restore()?;
        Ok(None)
    }

    fn handle_input(
        &mut self,
        state: &mut Editor,
        choices: &mut fz::Choices,
        byte: Option<u8>,
        resolve_ambiguous_key: bool,
    ) -> InputResult {
        state.ambiguous_key_pending = false;
        if let Some(byte) = byte {
            state.input.push(byte);
        }

        let mut matching_action = None;
        let mut in_middle = false;
        for &(key, action) in KEY_BINDINGS {
            if state.input == key {
                matching_action = Some(action);
            } else if key.starts_with(&state.input) {
                in_middle = true;
            }
        }

        if let Some(action) = matching_action {
            if !in_middle || resolve_ambiguous_key {
                state.input.clear();
                return self.apply_action(action, state, choices);
            }
        }

        if matching_action.is_some() && in_middle {
            state.ambiguous_key_pending = true;
            return InputResult::Continue {
                search_changed: false,
            };
        }

        if in_middle {
            return InputResult::Continue {
                search_changed: false,
            };
        }

        let mut search_changed = false;
        for index in 0..state.input.len() {
            let byte = state.input[index];
            if is_printable(byte) {
                search_changed |= state.insert(byte);
            }
        }
        state.input.clear();
        InputResult::Continue { search_changed }
    }

    fn apply_action(
        &mut self,
        action: Action,
        state: &mut Editor,
        choices: &mut fz::Choices,
    ) -> InputResult {
        if matches!(action, Action::Accept | Action::Complete | Action::Previous | Action::Next | Action::PageUp | Action::PageDown) {
            state.update_choices(choices);
        }
        let changed = match action {
            Action::Cancel => return InputResult::Cancel,
            Action::Accept => return InputResult::Accept,
            Action::DeleteCharacter => state.delete_character(),
            Action::DeleteWord => state.delete_word(),
            Action::DeleteToBeginning => state.delete_to_beginning(),
            Action::Complete => choices.selected().is_some_and(|candidate| state.replace(candidate)),
            Action::Previous => { choices.prev(); false }
            Action::Next => { choices.next(); false }
            Action::Left => { state.left(); false }
            Action::Right => { state.right(); false }
            Action::Beginning => { state.cursor = 0; false }
            Action::End => { state.cursor = state.query.len(); false }
            Action::PageUp => {
                for _ in 0..state.lines.min(choices.selection()) { choices.prev(); }
                false
            }
            Action::PageDown => {
                let remaining = choices.available().saturating_sub(choices.selection() + 1);
                for _ in 0..state.lines.min(remaining) { choices.next(); }
                false
            }
            Action::Ignore => false,
        };
        InputResult::Continue { search_changed: changed }
    }

    fn draw(
        &mut self,
        choices: &fz::Choices,
        options: &crate::Options,
        state: &Editor,
    ) -> io::Result<()> {
        let available = choices.available();
        let selection = choices.selection();
        let mut start = 0usize;
        if state.lines > 0 && selection.saturating_add(1) >= state.lines {
            start = selection.saturating_add(2).saturating_sub(state.lines);
            if start.saturating_add(state.lines) >= available && available > 0 {
                start = available.saturating_sub(state.lines);
            }
        }

        self.set_column(0)?;
        self.write_bytes(&options.prompt)?;
        self.write_bytes(&state.query)?;
        self.clear_line()?;

        if options.show_info {
            self.write_bytes(b"\n[")?;
            self.write_usize(available)?;
            self.write_bytes(b"/")?;
            self.write_usize(choices.len())?;
            self.write_bytes(b"]")?;
            self.clear_line()?;
        }

        for index in start..start.saturating_add(state.lines) {
            self.write_bytes(b"\n")?;
            self.clear_line()?;
            if let Some(candidate) = choices.get(index) {
                let score = options.show_scores.then(|| choices.getscore(index).unwrap());
                self.draw_candidate(candidate, state, score, index == selection)?;
            }
        }

        let result_rows = state.lines + usize::from(options.show_info);
        if result_rows > 0 {
            self.move_up(result_rows)?;
        }
        self.set_column(0)?;
        self.write_bytes(&options.prompt)?;
        self.write_bytes(&state.query[..state.cursor])?;
        self.flush()
    }

    fn draw_candidate(
        &mut self,
        candidate: &[u8],
        state: &Editor,
        score: Option<f64>,
        selected: bool,
    ) -> io::Result<()> {
        if let Some(score) = score {
            if score == f64::NEG_INFINITY {
                self.write_bytes(b"(     ) ")?;
            } else {
                write!(self.output, "({score:5.2}) ")?;
            }
        }

        if selected {
            self.set_invert()?;
        }
        self.set_nowrap()?;

        let positions = fz::match_positions(&state.query, candidate).unwrap_or_default();
        let mut position = 0usize;
        for (index, &byte) in candidate.iter().enumerate() {
            if positions.get(position) == Some(&index) {
                self.set_foreground(3)?;
                position += 1;
            } else {
                self.set_foreground(9)?;
            }
            if byte == b'\n' {
                self.write_bytes(b" ")?;
            } else {
                self.output.write_all(&candidate[index..=index])?;
            }
        }

        self.set_wrap()?;
        self.set_normal()
    }

    fn clear(&mut self, lines: usize, show_info: bool) -> io::Result<()> {
        self.set_column(0)?;
        for _ in 0..lines + usize::from(show_info) {
            self.newline()?;
        }
        self.clear_line()?;
        if lines > 0 {
            self.move_up(lines + usize::from(show_info))?;
        }
        self.flush()
    }

    fn dimensions(&self) -> (usize, usize) {
        let Ok(size) = termios::tcgetwinsize(self.output.get_ref()) else {
            return (80, 25);
        };
        if size.ws_col == 0 || size.ws_row == 0 {
            return (80, 25);
        }
        (usize::from(size.ws_col), usize::from(size.ws_row))
    }

    fn wait_for_input(&self, timeout: Option<Duration>) -> io::Result<Wait> {
        self.resize_events.wait_for_input(&self.input, timeout)
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        loop {
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let termios_result = if self.restore_needed {
            self.restore_needed = false;
            termios::tcsetattr(&self.input, OptionalActions::Now, &self.original)
                .map_err(io::Error::from)
        } else {
            Ok(())
        };
        let resize_result = self.resize_events.restore();
        termios_result.and(resize_result)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)
    }

    fn write_usize(&mut self, value: usize) -> io::Result<()> {
        write!(self.output, "{value}")
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }

    fn set_foreground(&mut self, color: u8) -> io::Result<()> {
        if self.foreground != color {
            write!(self.output, "\x1b[{}m", 30 + color)?;
            self.foreground = color;
        }
        Ok(())
    }

    fn set_invert(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[7m")
    }

    fn set_normal(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[0m")?;
        self.foreground = 9;
        Ok(())
    }

    fn set_nowrap(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[?7l")
    }

    fn set_wrap(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[?7h")
    }

    fn clear_line(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[K")
    }

    fn newline(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[K\n")
    }

    fn set_column(&mut self, column: usize) -> io::Result<()> {
        write!(self.output, "\x1b[{}G", column + 1)
    }

    fn move_up(&mut self, rows: usize) -> io::Result<()> {
        write!(self.output, "\x1b[{}A", rows)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone, Copy)]
enum Action {
    Cancel,
    Accept,
    DeleteCharacter,
    DeleteWord,
    DeleteToBeginning,
    Complete,
    Previous,
    Next,
    Left,
    Right,
    Beginning,
    End,
    PageUp,
    PageDown,
    Ignore,
}

const KEY_BINDINGS: &[(&[u8], Action)] = &[
    (b"\x1b", Action::Cancel),
    (b"\x7f", Action::DeleteCharacter),
    (b"\x08", Action::DeleteCharacter),
    (b"\x17", Action::DeleteWord),
    (b"\x15", Action::DeleteToBeginning),
    (b"\x09", Action::Complete),
    (b"\x03", Action::Cancel),
    (b"\x04", Action::Cancel),
    (b"\x07", Action::Cancel),
    (b"\x0d", Action::Accept),
    (b"\x10", Action::Previous),
    (b"\x0e", Action::Next),
    (b"\x0b", Action::Previous),
    (b"\x0a", Action::Next),
    (b"\x01", Action::Beginning),
    (b"\x05", Action::End),
    (b"\x1bOD", Action::Left),
    (b"\x1b[D", Action::Left),
    (b"\x1bOC", Action::Right),
    (b"\x1b[C", Action::Right),
    (b"\x1b[1~", Action::Beginning),
    (b"\x1b[H", Action::Beginning),
    (b"\x1b[4~", Action::End),
    (b"\x1b[F", Action::End),
    (b"\x1b[A", Action::Previous),
    (b"\x1bOA", Action::Previous),
    (b"\x1b[B", Action::Next),
    (b"\x1bOB", Action::Next),
    (b"\x1b[5~", Action::PageUp),
    (b"\x1b[6~", Action::PageDown),
    (b"\x1b[200~", Action::Ignore),
    (b"\x1b[201~", Action::Ignore),
];

enum Wait {
    Ready,
    TimedOut,
    Interrupted,
}

enum InputResult {
    Continue { search_changed: bool },
    Accept,
    Cancel,
}

struct Editor {
    query: Vec<u8>,
    cursor: usize,
    input: Vec<u8>,
    ambiguous_key_pending: bool,
    search_dirty: bool,
    lines: usize,
}

impl Editor {
    fn new(mut query: Vec<u8>, lines: usize) -> Self {
        query.truncate(4096);
        let cursor = query.len();
        Self {
            query,
            cursor,
            input: Vec::new(),
            ambiguous_key_pending: false,
            search_dirty: false,
            lines,
        }
    }

    fn update_choices(&mut self, choices: &mut fz::Choices) {
        if self.search_dirty {
            choices.search(&self.query);
            self.search_dirty = false;
        }
    }

    fn insert(&mut self, byte: u8) -> bool {
        if self.query.len() == 4096 {
            return false;
        }
        self.query.insert(self.cursor, byte);
        self.cursor += 1;
        true
    }

    fn replace(&mut self, candidate: &[u8]) -> bool {
        let end = candidate.len().min(4096);
        if self.query == candidate[..end] {
            self.cursor = end;
            return false;
        }
        self.query.clear();
        self.query.extend_from_slice(&candidate[..end]);
        self.cursor = end;
        true
    }

    fn delete_character(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = previous_boundary(&self.query, self.cursor);
        self.query.drain(start..self.cursor);
        self.cursor = start;
        true
    }

    fn delete_word(&mut self) -> bool {
        let original = self.cursor;
        let mut cursor = original;
        while cursor > 0 && is_ascii_space(self.query[cursor - 1]) {
            cursor -= 1;
        }
        while cursor > 0 && !is_ascii_space(self.query[cursor - 1]) {
            cursor -= 1;
        }
        if cursor == original {
            return false;
        }
        self.query.drain(cursor..original);
        self.cursor = cursor;
        true
    }

    fn delete_to_beginning(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.query.drain(..self.cursor);
        self.cursor = 0;
        true
    }

    fn left(&mut self) {
        self.cursor = previous_boundary(&self.query, self.cursor);
    }

    fn right(&mut self) {
        self.cursor = next_boundary(&self.query, self.cursor);
    }
}

fn is_printable(byte: u8) -> bool {
    byte >= 0x80 || (0x20..=0x7e).contains(&byte)
}

fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn previous_boundary(bytes: &[u8], cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut position = cursor - 1;
    while position > 0 && is_continuation(bytes[position]) {
        position -= 1;
    }
    position
}

fn next_boundary(bytes: &[u8], cursor: usize) -> usize {
    if cursor >= bytes.len() {
        return bytes.len();
    }
    let mut position = cursor + 1;
    while position < bytes.len() && is_continuation(bytes[position]) {
        position += 1;
    }
    position
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::ResizeEvents;
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;

    struct RestoreMask(libc::sigset_t);

    impl Drop for RestoreMask {
        fn drop(&mut self) {
            // The saved mask belongs to this test thread. Test cleanup must
            // not leak its temporary signal state into the harness.
            let _ = unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut())
            };
        }
    }

    fn current_mask() -> io::Result<libc::sigset_t> {
        // libc may leave unused sigset_t storage untouched; a zero mask is a
        // valid initial representation for that storage.
        let mut mask = MaybeUninit::<libc::sigset_t>::zeroed();
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), mask.as_mut_ptr())
        };
        if result == 0 {
            // SAFETY: pthread_sigmask filled its kernel-visible prefix and
            // the zeroed remainder was already initialized.
            Ok(unsafe { mask.assume_init() })
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn signal_set(signal: libc::c_int) -> io::Result<libc::sigset_t> {
        let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
        if unsafe { libc::sigemptyset(set.as_mut_ptr()) } == -1
            || unsafe { libc::sigaddset(set.as_mut_ptr(), signal) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed storage is a valid empty mask, and libc updated its
        // required signal-set prefix.
        Ok(unsafe { set.assume_init() })
    }

    fn change_signal_mask(how: libc::c_int, signal: libc::c_int) -> io::Result<()> {
        let set = signal_set(signal)?;
        let result = unsafe { libc::pthread_sigmask(how, &set, std::ptr::null_mut()) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn is_blocked(mask: &libc::sigset_t, signal: libc::c_int) -> bool {
        match unsafe { libc::sigismember(mask, signal) } {
            0 => false,
            1 => true,
            _ => panic!("sigismember({signal}) failed: {}", io::Error::last_os_error()),
        }
    }

    #[test]
    fn resize_events_restore_blocked_and_unblocked_caller_masks() -> io::Result<()> {
        let original = current_mask()?;
        let _restore_original = RestoreMask(original);

        // Preserve both legal caller states. A parent may already block
        // SIGWINCH, while an unrelated blocked signal proves restoration uses
        // the complete saved mask rather than only unblocking SIGWINCH.
        change_signal_mask(libc::SIG_BLOCK, libc::SIGUSR1)?;
        for caller_blocks_winch in [false, true] {
            change_signal_mask(
                if caller_blocks_winch {
                    libc::SIG_BLOCK
                } else {
                    libc::SIG_UNBLOCK
                },
                libc::SIGWINCH,
            )?;
            let expected_after_restore = current_mask()?;

            let input = File::open("/dev/null")?;
            let mut events = ResizeEvents::install(&input)?;
            let installed_mask = current_mask()?;
            assert!(is_blocked(&installed_mask, libc::SIGWINCH));
            assert!(is_blocked(&installed_mask, libc::SIGUSR1));

            events.restore()?;
            let restored_mask = current_mask()?;
            assert_eq!(
                is_blocked(&restored_mask, libc::SIGWINCH),
                caller_blocks_winch
            );
            assert_eq!(
                is_blocked(&restored_mask, libc::SIGUSR1),
                is_blocked(&expected_after_restore, libc::SIGUSR1)
            );
        }
        Ok(())
    }
}
