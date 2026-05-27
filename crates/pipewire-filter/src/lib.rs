//! minimal safe wrapper around `pw_filter` (pipewire-rs 0.8 has no
//! `Filter`). mirrors `pipewire::stream`; lives in its own crate so
//! the unsafe FFI stays out of `headroom-core`'s `forbid(unsafe_code)`.
//! every `unsafe` block carries a `// SAFETY:` comment.
//!
//! drop order invariant: [`Filter`] must outlive its [`FilterListener`]
//! — otherwise a trampoline could recover a freed `Box`. not encoded in
//! the types (same as pipewire-rs `Stream`); callers drop the listener
//! first.

#![warn(missing_docs)]
#![warn(clippy::missing_safety_doc)]

pub mod error;

use std::ffi::CString;
use std::marker::PhantomData;
use std::mem;
use std::os::raw::c_void;
use std::pin::Pin;
use std::ptr::NonNull;

use pipewire::{
    core::Core,
    properties::Properties,
};

pub use error::FilterError;
pub use libspa::utils::Direction;

/// state of a [`Filter`], from the C `pw_filter_state` enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterState {
    /// `PW_FILTER_STATE_ERROR`. carries the optional error string.
    Error(String),
    /// `PW_FILTER_STATE_UNCONNECTED`.
    Unconnected,
    /// `PW_FILTER_STATE_CONNECTING`.
    Connecting,
    /// `PW_FILTER_STATE_PAUSED`.
    Paused,
    /// `PW_FILTER_STATE_STREAMING`.
    Streaming,
}

impl FilterState {
    /// decode the C enum + optional error string into [`FilterState`].
    ///
    /// # Safety
    /// `error` must be NULL or a NUL-terminated C string valid until
    /// return (PipeWire guarantees both for the callback's duration).
    unsafe fn from_raw(state: pipewire_sys::pw_filter_state, error: *const std::os::raw::c_char) -> Self {
        match state {
            pipewire_sys::pw_filter_state_PW_FILTER_STATE_UNCONNECTED => Self::Unconnected,
            pipewire_sys::pw_filter_state_PW_FILTER_STATE_CONNECTING => Self::Connecting,
            pipewire_sys::pw_filter_state_PW_FILTER_STATE_PAUSED => Self::Paused,
            pipewire_sys::pw_filter_state_PW_FILTER_STATE_STREAMING => Self::Streaming,
            _ => {
                let msg = if error.is_null() {
                    String::new()
                } else {
                    // SAFETY: documented above; PipeWire guarantees a
                    // valid NUL-terminated string for the call's
                    // duration.
                    std::ffi::CStr::from_ptr(error)
                        .to_string_lossy()
                        .into_owned()
                };
                Self::Error(msg)
            }
        }
    }
}

/// flags accepted by [`Filter::connect`]. mirrors `enum pw_filter_flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterFlags(pipewire_sys::pw_filter_flags);

impl FilterFlags {
    /// `PW_FILTER_FLAG_NONE`.
    pub const NONE: Self = Self(pipewire_sys::pw_filter_flags_PW_FILTER_FLAG_NONE);
    /// `PW_FILTER_FLAG_INACTIVE`.
    pub const INACTIVE: Self = Self(pipewire_sys::pw_filter_flags_PW_FILTER_FLAG_INACTIVE);
    /// `PW_FILTER_FLAG_DRIVER`.
    pub const DRIVER: Self = Self(pipewire_sys::pw_filter_flags_PW_FILTER_FLAG_DRIVER);
    /// `PW_FILTER_FLAG_RT_PROCESS` — process on the rt data thread.
    pub const RT_PROCESS: Self = Self(pipewire_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS);
    /// `PW_FILTER_FLAG_CUSTOM_LATENCY`.
    pub const CUSTOM_LATENCY: Self =
        Self(pipewire_sys::pw_filter_flags_PW_FILTER_FLAG_CUSTOM_LATENCY);

    /// OR two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// raw bits for `pw_filter_connect`.
    #[must_use]
    pub const fn bits(self) -> pipewire_sys::pw_filter_flags {
        self.0
    }
}

impl std::ops::BitOr for FilterFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// flags accepted by [`Filter::add_port`]. mirrors
/// `enum pw_filter_port_flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PortFlags(pipewire_sys::pw_filter_port_flags);

impl PortFlags {
    /// `PW_FILTER_PORT_FLAG_NONE`.
    pub const NONE: Self = Self(pipewire_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_NONE);
    /// `PW_FILTER_PORT_FLAG_MAP_BUFFERS` — mmap so `data.data` is
    /// directly readable/writable.
    pub const MAP_BUFFERS: Self =
        Self(pipewire_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS);

    /// OR two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// raw bits for `pw_filter_add_port`.
    #[must_use]
    pub const fn bits(self) -> pipewire_sys::pw_filter_port_flags {
        self.0
    }
}

impl std::ops::BitOr for PortFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// owns a `*mut pw_filter`. drop calls `pw_filter_destroy`.
///
/// not `Send`/`Sync`: `pw_filter` is bound to its owning main loop,
/// like `pw_stream`.
pub struct Filter {
    ptr: NonNull<pipewire_sys::pw_filter>,
    /// keeps the Core alive while this filter exists.
    _core: Core,
    /// `!Send`/`!Sync` even where `NonNull<_>` is `Send`.
    _not_send: PhantomData<*mut ()>,
}

impl Filter {
    /// create an unconnected filter. `properties` is consumed (PipeWire
    /// takes ownership of the `pw_properties`).
    ///
    /// # Errors
    /// [`FilterError::CreationFailed`] if `pw_filter_new` returns NULL.
    pub fn new(core: &Core, name: &str, properties: Properties) -> Result<Self, FilterError> {
        let c_name = CString::new(name).expect("filter name contains a NUL byte");
        // SAFETY:
        //  - `core.as_raw_ptr()` returns the live `*mut pw_core` that
        //    `Core` keeps alive for the lifetime of the reference.
        //  - `c_name.as_ptr()` is valid through this expression.
        //  - `properties.into_raw()` consumes ownership; PipeWire
        //    must free the `pw_properties` (it does: per filter.h
        //    "ownership is taken"). We must NOT free it ourselves;
        //    `into_raw` is exactly the API for that handoff.
        let ptr = unsafe {
            pipewire_sys::pw_filter_new(core.as_raw_ptr(), c_name.as_ptr(), properties.into_raw())
        };
        let ptr = NonNull::new(ptr).ok_or(FilterError::CreationFailed)?;
        Ok(Self {
            ptr,
            _core: core.clone(),
            _not_send: PhantomData,
        })
    }

    fn as_raw_ptr(&self) -> *mut pipewire_sys::pw_filter {
        self.ptr.as_ptr()
    }

    /// add a port. `params` are borrowed PODs (initial format hint);
    /// `properties` is consumed (as [`Self::new`]). `port_data_size`
    /// is 0 — the rt callback recovers state from the listener box.
    ///
    /// # Errors
    /// [`FilterError::AddPortFailed`] if `pw_filter_add_port` returns
    /// NULL.
    pub fn add_port(
        &self,
        direction: Direction,
        flags: PortFlags,
        properties: Properties,
        params: &mut [&libspa::pod::Pod],
    ) -> Result<PortData, FilterError> {
        // SAFETY:
        //  - `self.as_raw_ptr()` is valid for the lifetime of `self`.
        //  - `direction.as_raw()` is one of SPA_DIRECTION_INPUT /
        //    SPA_DIRECTION_OUTPUT.
        //  - `properties.into_raw()` hands ownership over; PipeWire
        //    frees on filter destruction.
        //  - `params` is `&mut [&Pod]`. `Pod` is `#[repr(transparent)]`
        //    over `spa_pod`, so `&Pod` and `*const spa_pod` have the
        //    same layout. The cast pattern is the one pipewire-rs
        //    uses for `pw_stream_connect` (stream.rs:170).
        //  - `params` is not stored; PipeWire copies whatever it needs
        //    from the PODs before returning.
        let port_data = unsafe {
            pipewire_sys::pw_filter_add_port(
                self.as_raw_ptr(),
                direction.as_raw(),
                flags.bits(),
                0,
                properties.into_raw(),
                params.as_mut_ptr().cast(),
                params.len() as u32,
            )
        };
        let port_data = NonNull::new(port_data).ok_or(FilterError::AddPortFailed)?;
        Ok(PortData { ptr: port_data })
    }

    /// connect the filter for processing. `params` are optional
    /// filter-level format hints (headroom passes none; negotiation is
    /// per-port).
    ///
    /// # Errors
    /// [`FilterError::ConnectFailed`] if `pw_filter_connect` returns a
    /// negative result code.
    pub fn connect(
        &self,
        flags: FilterFlags,
        params: &mut [&libspa::pod::Pod],
    ) -> Result<(), FilterError> {
        // SAFETY: same argument-validity rationale as `add_port`. The
        // params slice can be empty — pipewire-rs's `Stream::connect`
        // accepts the same.
        let rc = unsafe {
            pipewire_sys::pw_filter_connect(
                self.as_raw_ptr(),
                flags.bits(),
                params.as_mut_ptr().cast(),
                params.len() as u32,
            )
        };
        if rc < 0 {
            let errno = -rc;
            return Err(FilterError::ConnectFailed(std::io::Error::from_raw_os_error(
                errno,
            )));
        }
        Ok(())
    }

    /// node id assigned by the server; non-zero once connected + acked.
    #[must_use]
    pub fn node_id(&self) -> u32 {
        // SAFETY: `self.as_raw_ptr()` is valid for the lifetime of
        // `self`. `pw_filter_get_node_id` is documented as a simple
        // getter, no side effects.
        unsafe { pipewire_sys::pw_filter_get_node_id(self.as_raw_ptr()) }
    }

    /// begin registering a listener. `user_data` is moved into the
    /// listener box, reachable from every callback as `&mut D`.
    /// finalise with [`ListenerBuilder::register`].
    pub fn add_local_listener_with_user_data<D>(
        &self,
        user_data: D,
    ) -> ListenerBuilder<'_, D> {
        ListenerBuilder {
            filter: self,
            callbacks: ListenerCallbacks::with_user_data(user_data),
        }
    }
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filter")
            .field("node_id", &self.node_id())
            .finish()
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was returned by `pw_filter_new` and has
        // not been freed since. `pw_filter_destroy` is the documented
        // cleanup function. Note: callers are expected to drop any
        // associated `FilterListener` first; otherwise the listener's
        // `spa_hook::remove` runs against a freed list. We can't
        // express that constraint in the borrow checker without
        // making the listener literally borrow the filter, which
        // pipewire-rs `Stream` also chooses not to do.
        unsafe { pipewire_sys::pw_filter_destroy(self.as_raw_ptr()) }
    }
}

/// opaque port handle from [`Filter::add_port`]; rt callbacks move
/// audio via [`Self::dequeue_buffer`] + the [`Buffer`] RAII.
///
/// `Send` so the listener user-data struct can own port handles, but
/// not `Sync`: `pw_filter_dequeue_buffer` writes a per-port lockless
/// ring, so concurrent same-port calls aren't documented-safe. all
/// real use is single-threaded in the rt callback.
pub struct PortData {
    ptr: NonNull<c_void>,
}

// SAFETY: the underlying `*mut c_void` points into the `pw_filter`
// allocation, which lives until `pw_filter_destroy`. The Filter
// owns the lifetime; the documented drop order (listener before
// filter) ensures that PortData inside the listener's user-data
// box never outlives the filter. Move-only ownership is the right
// model — there must be exactly one logical owner of a port
// handle, and the RT-callback closure is where that owner lives.
unsafe impl Send for PortData {}

impl PortData {
    fn as_raw_ptr(&self) -> *mut c_void {
        self.ptr.as_ptr()
    }

    /// dequeue the next buffer (fresh data on input ports, blank on
    /// output ports). `None` if the queue is empty. the returned
    /// [`Buffer`] queues itself back on drop.
    pub fn dequeue_buffer(&self) -> Option<Buffer<'_>> {
        // SAFETY: `as_raw_ptr` returns the live port handle; PipeWire
        // returns either a valid `*mut pw_buffer` or NULL. The
        // returned pointer is owned by PipeWire — we only borrow it
        // for the realtime callback's duration. The `Buffer` RAII
        // hands it back via `pw_filter_queue_buffer` on drop.
        let raw = unsafe { pipewire_sys::pw_filter_dequeue_buffer(self.as_raw_ptr()) };
        let raw = NonNull::new(raw)?;
        Some(Buffer {
            buf: raw,
            port: self,
        })
    }
}

/// RAII handle for a dequeued buffer. drop calls `pw_filter_queue_buffer`.
pub struct Buffer<'p> {
    buf: NonNull<pipewire_sys::pw_buffer>,
    port: &'p PortData,
}

impl Buffer<'_> {
    /// borrow the buffer's `spa_data` slice (mirrors pipewire-rs
    /// `Buffer::datas_mut`).
    pub fn datas_mut(&mut self) -> &mut [libspa::buffer::Data] {
        // SAFETY: `pw_buffer.buffer` points at a `spa_buffer` that
        // PipeWire owns for the duration of this callback. The same
        // invariant pipewire-rs relies on in `Buffer::datas_mut`. If
        // `n_datas == 0` or `datas == NULL` we return an empty slice
        // rather than dereferencing.
        unsafe {
            let pw_buf = self.buf.as_ptr();
            let spa_buf = (*pw_buf).buffer;
            if spa_buf.is_null() {
                return &mut [];
            }
            let n_datas = (*spa_buf).n_datas;
            let datas_ptr = (*spa_buf).datas;
            if n_datas == 0 || datas_ptr.is_null() {
                return &mut [];
            }
            // `libspa::buffer::Data` is `#[repr(transparent)]` over
            // `spa_sys::spa_data`, so a `*mut spa_data` is layout-
            // compatible with `*mut Data`.
            let datas = datas_ptr.cast::<libspa::buffer::Data>();
            std::slice::from_raw_parts_mut(datas, n_datas as usize)
        }
    }
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.buf` was obtained from
        // `pw_filter_dequeue_buffer` on `self.port` and has not been
        // queued elsewhere (this is the only path that consumes it).
        // The `&PortData` borrow keeps the port alive at least until
        // this call returns.
        unsafe {
            pipewire_sys::pw_filter_queue_buffer(self.port.as_raw_ptr(), self.buf.as_ptr());
        }
    }
}

// -- Listener machinery ------------------------------------------------------

type ProcessCb<D> = dyn FnMut(&mut D, *mut libspa_sys::spa_io_position);
type StateChangedCb<D> = dyn FnMut(&mut D, FilterState, FilterState);
type ParamChangedCb<D> = dyn FnMut(&mut D, *mut c_void, u32, Option<&libspa::pod::Pod>);

/// user data + per-event closures, behind a `Box` whose raw pointer is
/// the trampoline's `data` argument.
struct ListenerCallbacks<D> {
    user_data: D,
    process: Option<Box<ProcessCb<D>>>,
    state_changed: Option<Box<StateChangedCb<D>>>,
    param_changed: Option<Box<ParamChangedCb<D>>>,
}

impl<D> ListenerCallbacks<D> {
    fn with_user_data(user_data: D) -> Self {
        Self {
            user_data,
            process: None,
            state_changed: None,
            param_changed: None,
        }
    }

    /// build the C `pw_filter_events` vtable + heap-boxed callbacks.
    /// only wires events whose closure is set.
    fn into_raw(self) -> (Pin<Box<pipewire_sys::pw_filter_events>>, Box<Self>) {
        let callbacks = Box::new(self);

        // SAFETY notes for the trampolines below:
        //  - `data` is the `*mut c_void` we hand to
        //    `pw_filter_add_listener`. It is the raw pointer to the
        //    `Box<ListenerCallbacks<D>>`. The box is reclaimed by the
        //    `FilterListener` on drop, so during the listener's
        //    lifetime the pointer remains valid.
        //  - We rebuild a `&mut ListenerCallbacks<D>` from `data`,
        //    NOT a `Box<_>`. We must not double-free.
        //  - PipeWire serialises callbacks for a single filter on a
        //    single thread (data thread for RT events, main loop
        //    otherwise) so the unique borrow is sound.

        unsafe extern "C" fn on_process<D>(
            data: *mut c_void,
            position: *mut libspa_sys::spa_io_position,
        ) {
            // SAFETY: per the block comment above.
            let state = unsafe { &mut *(data as *mut ListenerCallbacks<D>) };
            if let Some(cb) = &mut state.process {
                cb(&mut state.user_data, position);
            }
        }

        unsafe extern "C" fn on_state_changed<D>(
            data: *mut c_void,
            old: pipewire_sys::pw_filter_state,
            new: pipewire_sys::pw_filter_state,
            error: *const std::os::raw::c_char,
        ) {
            // SAFETY: per the block comment above.
            let state = unsafe { &mut *(data as *mut ListenerCallbacks<D>) };
            if let Some(cb) = &mut state.state_changed {
                // SAFETY for `new`: error is documented to either be
                // NULL or a valid NUL-terminated C string owned by
                // the filter.
                let new = unsafe { FilterState::from_raw(new, error) };
                // `error` only describes the *new* state; passing it
                // to the `old` decode would misattribute the message
                // if a future PipeWire enum value falls through to
                // the `_` arm.
                let old = unsafe { FilterState::from_raw(old, std::ptr::null()) };
                cb(&mut state.user_data, old, new);
            }
        }

        unsafe extern "C" fn on_param_changed<D>(
            data: *mut c_void,
            port_data: *mut c_void,
            id: u32,
            param: *const libspa_sys::spa_pod,
        ) {
            // SAFETY: per the block comment above.
            let state = unsafe { &mut *(data as *mut ListenerCallbacks<D>) };
            if let Some(cb) = &mut state.param_changed {
                let param_ref = if param.is_null() {
                    None
                } else {
                    // SAFETY: PipeWire owns the POD for the call's
                    // duration. `Pod::from_raw` only borrows.
                    Some(unsafe { libspa::pod::Pod::from_raw(param) })
                };
                cb(&mut state.user_data, port_data, id, param_ref);
            }
        }

        // SAFETY: `mem::zeroed` produces an all-NULL `pw_filter_events`
        // — every callback field is `Option<unsafe extern "C" fn ...>`
        // which is layout-equivalent to a nullable function pointer.
        // We then fill in the fields we want and leave the rest NULL,
        // which is exactly what PipeWire expects (it skips NULL slots).
        let events = unsafe {
            let mut events: Pin<Box<pipewire_sys::pw_filter_events>> = Box::pin(mem::zeroed());
            events.version = pipewire_sys::PW_VERSION_FILTER_EVENTS;
            if callbacks.process.is_some() {
                events.process = Some(on_process::<D>);
            }
            if callbacks.state_changed.is_some() {
                events.state_changed = Some(on_state_changed::<D>);
            }
            if callbacks.param_changed.is_some() {
                events.param_changed = Some(on_param_changed::<D>);
            }
            events
        };

        (events, callbacks)
    }
}

/// builder from [`Filter::add_local_listener_with_user_data`]. install
/// closures, then [`Self::register`].
#[must_use = "Listener builders do nothing until .register() is called"]
pub struct ListenerBuilder<'f, D> {
    filter: &'f Filter,
    callbacks: ListenerCallbacks<D>,
}

impl<D> ListenerBuilder<'_, D> {
    /// set the rt process callback.
    pub fn process<F>(mut self, callback: F) -> Self
    where
        F: FnMut(&mut D, *mut libspa_sys::spa_io_position) + 'static,
    {
        self.callbacks.process = Some(Box::new(callback));
        self
    }

    /// set the state-changed callback.
    pub fn state_changed<F>(mut self, callback: F) -> Self
    where
        F: FnMut(&mut D, FilterState, FilterState) + 'static,
    {
        self.callbacks.state_changed = Some(Box::new(callback));
        self
    }

    /// set the param-changed callback. `port_data` is NULL for
    /// filter-level changes, else the per-port handle; the `Pod` is
    /// borrowed for the call.
    pub fn param_changed<F>(mut self, callback: F) -> Self
    where
        F: FnMut(&mut D, *mut c_void, u32, Option<&libspa::pod::Pod>) + 'static,
    {
        self.callbacks.param_changed = Some(Box::new(callback));
        self
    }

    /// register the listener; drop the returned [`FilterListener`] to
    /// unregister.
    ///
    /// # Errors
    /// never (`pw_filter_add_listener` is `void`); `Result` kept for
    /// parity with pipewire-rs `Stream::register`.
    pub fn register(self) -> Result<FilterListener<D>, FilterError> {
        let (events, data) = self.callbacks.into_raw();
        // SAFETY:
        //  - `Box::into_raw` consumes the box, leaving the heap
        //    allocation alive. We reclaim it inside the
        //    `FilterListener` on drop.
        //  - The events table is `Box::pin`ned; the raw `&` returned
        //    by `events.as_ref().get_ref()` is stable for as long as
        //    the listener holds the pinned box (the listener owns it).
        //  - The spa_hook is zero-initialised and handed to PipeWire
        //    to populate.
        let (listener, data) = unsafe {
            let listener: Box<libspa_sys::spa_hook> = Box::new(mem::zeroed());
            let raw_listener = Box::into_raw(listener);
            let raw_data = Box::into_raw(data);
            pipewire_sys::pw_filter_add_listener(
                self.filter.as_raw_ptr(),
                raw_listener,
                events.as_ref().get_ref(),
                raw_data.cast(),
            );
            (Box::from_raw(raw_listener), Box::from_raw(raw_data))
        };
        Ok(FilterListener {
            listener,
            _events: events,
            _data: data,
        })
    }
}

/// owns the spa_hook + heap-boxed callbacks; drop unhooks the listener.
/// must outlive any callback — drop this *before* the [`Filter`], else
/// `pw_filter_destroy` could fire a trampoline against a freed hook.
pub struct FilterListener<D> {
    listener: Box<libspa_sys::spa_hook>,
    /// pinned: PipeWire keeps a pointer into this allocation.
    _events: Pin<Box<pipewire_sys::pw_filter_events>>,
    /// the trampoline's `data` box; kept alive for the listener's life.
    _data: Box<ListenerCallbacks<D>>,
}

impl<D> Drop for FilterListener<D> {
    fn drop(&mut self) {
        // SAFETY: `self.listener` is the spa_hook PipeWire wrote into
        // during `pw_filter_add_listener`. `hook::remove` consumes the
        // hook by value; we hand it a copy from the box, then the box
        // itself is freed by the auto-generated Drop. The original
        // hook in `self.listener` is now invalid but no further code
        // reads it.
        let hook = *self.listener;
        libspa::utils::hook::remove(hook);
    }
}
