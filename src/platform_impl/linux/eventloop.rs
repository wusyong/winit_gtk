use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    ffi::c_void,
    process,
    ptr::NonNull,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
    time::Instant,
};

use cairo::{RectangleInt, Region};
use crossbeam_channel::SendError;
use gdk::{
    prelude::{ApplicationExt, DisplayExtManual},
    Cursor, CursorType, EventKey, EventMask, ScrollDirection, WindowEdge, WindowState,
};
use gio::Cancellable;
use glib::{MainContext, ObjectType, Priority};
use gtk::{
    prelude::{DeviceExt, SeatExt, WidgetExtManual},
    traits::{GtkApplicationExt, GtkWindowExt, WidgetExt},
};
use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle, XlibDisplayHandle};

use crate::{
    dpi::{LogicalPosition, LogicalSize},
    event::{
        ElementState, Event, KeyboardInput, ModifiersState, MouseButton, MouseScrollDelta,
        StartCause, TouchPhase, WindowEvent,
    },
    event_loop::{
        ControlFlow, DeviceEventFilter, EventLoopClosed, EventLoopWindowTarget as RootELW,
    },
    window::{CursorIcon, WindowId as RootWindowId},
};

use super::{
    keyboard,
    monitor::MonitorHandle,
    util,
    window::{hit_test, WindowRequest},
    Fullscreen, PlatformSpecificEventLoopAttributes, WindowId, DEVICE_ID,
};

pub struct EventLoop<T: 'static> {
    /// Window target.
    window_target: RootELW<T>,
    /// User event sender for EventLoopProxy
    pub(crate) user_event_tx: crossbeam_channel::Sender<Event<'static, T>>,
    /// Event queue of EventLoop
    events: crossbeam_channel::Receiver<Event<'static, T>>,
    /// Draw queue of EventLoop
    draws: crossbeam_channel::Receiver<WindowId>,
}

/// Used to send custom events to `EventLoop`.
#[derive(Debug)]
pub struct EventLoopProxy<T: 'static> {
    user_event_tx: crossbeam_channel::Sender<Event<'static, T>>,
}

impl<T: 'static> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        Self {
            user_event_tx: self.user_event_tx.clone(),
        }
    }
}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(_attributes: &PlatformSpecificEventLoopAttributes) -> Self {
        let context = MainContext::default();
        let app = gtk::Application::new(None, gio::ApplicationFlags::empty());
        let app_ = app.clone();
        let cancellable: Option<&Cancellable> = None;
        app.register(cancellable)
            .expect("Failed to register GtkApplication");

        // Create channels for handling events and send StartCause::Init event
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (draw_tx, draw_rx) = crossbeam_channel::unbounded();
        let event_tx_ = event_tx.clone();
        let draw_tx_ = draw_tx.clone();
        let user_event_tx = event_tx.clone();
        app.connect_activate(move |_| {
            if let Err(e) = event_tx_.send(Event::NewEvents(StartCause::Init)) {
                log::warn!("Failed to send init event to event channel: {}", e);
            }
        });

        // Create event loop window target.
        let (window_requests_tx, window_requests_rx) =
            glib::MainContext::channel(Priority::default());
        let display = gdk::Display::default()
            .expect("GdkDisplay not found. This usually means `gkt_init` hasn't called yet.");
        let window_target = EventLoopWindowTarget {
            display,
            app,
            windows: Rc::new(RefCell::new(HashSet::new())),
            window_requests_tx,
            draw_tx: draw_tx_,
            _marker: std::marker::PhantomData,
        };

        // TODO: Spawn x11/wayland thread to receive Device events.

        // Window Request
        window_requests_rx.attach(Some(&context), move |(id, request)| {
            if let Some(window) = app_.window_by_id(id.0 as u32) {
                match request {
                    WindowRequest::Title(title) => window.set_title(&title),
                    WindowRequest::Position((x, y)) => window.move_(x, y),
                    WindowRequest::Size((w, h)) => window.resize(w, h),
                    WindowRequest::SizeConstraints(min, max) => {
                        util::set_size_constraints(&window, min, max);
                    }
                    WindowRequest::Visible(visible) => {
                        if visible {
                            window.show_all();
                        } else {
                            window.hide();
                        }
                    }
                    WindowRequest::Focus => {
                        window.present_with_time(gdk_sys::GDK_CURRENT_TIME as _);
                    }
                    WindowRequest::Resizable(resizable) => window.set_resizable(resizable),
                    // WindowRequest::Closable(closable) => window.set_deletable(closable),
                    WindowRequest::Minimized(minimized) => {
                        if minimized {
                            window.iconify();
                        } else {
                            window.deiconify();
                        }
                    }
                    WindowRequest::Maximized(maximized) => {
                        if maximized {
                            window.maximize();
                        } else {
                            window.unmaximize();
                        }
                    }
                    WindowRequest::DragWindow => {
                        if let Some(cursor) = window
                            .display()
                            .default_seat()
                            .and_then(|seat| seat.pointer())
                        {
                            let (_, x, y) = cursor.position();
                            window.begin_move_drag(1, x, y, 0);
                        }
                    }
                    WindowRequest::Fullscreen(fullscreen) => match fullscreen {
                        Some(f) => {
                            let m = match f {
                                Fullscreen::Exclusive(m) => Some(m.monitor),
                                Fullscreen::Borderless(Some(m)) => Some(m.monitor),
                                _ => None,
                            };

                            if let Some(monitor) = m {
                                let display = window.display();
                                let monitors = display.n_monitors();
                                for i in 0..monitors {
                                    let m = display.monitor(i).unwrap();
                                    if m == monitor {
                                        let screen = display.default_screen();
                                        window.fullscreen_on_monitor(&screen, i);
                                    }
                                }
                            } else {
                                window.fullscreen();
                            }
                        }
                        None => window.unfullscreen(),
                    },
                    WindowRequest::Decorations(decorations) => window.set_decorated(decorations),
                    WindowRequest::AlwaysOnBottom(always_on_bottom) => {
                        window.set_keep_below(always_on_bottom)
                    }
                    WindowRequest::AlwaysOnTop(always_on_top) => {
                        window.set_keep_above(always_on_top)
                    }
                    WindowRequest::WindowIcon(window_icon) => {
                        if let Some(icon) = window_icon {
                            window.set_icon(Some(&icon.inner.into()));
                        }
                    }
                    WindowRequest::UserAttention(request_type) => {
                        window.set_urgency_hint(request_type.is_some())
                    }
                    WindowRequest::SetSkipTaskbar(skip) => {
                        window.set_skip_taskbar_hint(skip);
                        window.set_skip_pager_hint(skip)
                    }
                    // WindowRequest::SetVisibleOnAllWorkspaces(visible) => {
                    //     if visible {
                    //         window.stick();
                    //     } else {
                    //         window.unstick();
                    //     }
                    // }
                    WindowRequest::CursorIcon(cursor) => {
                        if let Some(gdk_window) = window.window() {
                            let display = window.display();
                            match cursor {
                                Some(cr) => gdk_window.set_cursor(
                                    Cursor::from_name(
                                        &display,
                                        match cr {
                                            CursorIcon::Crosshair => "crosshair",
                                            CursorIcon::Hand => "pointer",
                                            CursorIcon::Arrow => "arrow",
                                            CursorIcon::Move => "move",
                                            CursorIcon::Text => "text",
                                            CursorIcon::Wait => "wait",
                                            CursorIcon::Help => "help",
                                            CursorIcon::Progress => "progress",
                                            CursorIcon::NotAllowed => "not-allowed",
                                            CursorIcon::ContextMenu => "context-menu",
                                            CursorIcon::Cell => "cell",
                                            CursorIcon::VerticalText => "vertical-text",
                                            CursorIcon::Alias => "alias",
                                            CursorIcon::Copy => "copy",
                                            CursorIcon::NoDrop => "no-drop",
                                            CursorIcon::Grab => "grab",
                                            CursorIcon::Grabbing => "grabbing",
                                            CursorIcon::AllScroll => "all-scroll",
                                            CursorIcon::ZoomIn => "zoom-in",
                                            CursorIcon::ZoomOut => "zoom-out",
                                            CursorIcon::EResize => "e-resize",
                                            CursorIcon::NResize => "n-resize",
                                            CursorIcon::NeResize => "ne-resize",
                                            CursorIcon::NwResize => "nw-resize",
                                            CursorIcon::SResize => "s-resize",
                                            CursorIcon::SeResize => "se-resize",
                                            CursorIcon::SwResize => "sw-resize",
                                            CursorIcon::WResize => "w-resize",
                                            CursorIcon::EwResize => "ew-resize",
                                            CursorIcon::NsResize => "ns-resize",
                                            CursorIcon::NeswResize => "nesw-resize",
                                            CursorIcon::NwseResize => "nwse-resize",
                                            CursorIcon::ColResize => "col-resize",
                                            CursorIcon::RowResize => "row-resize",
                                            CursorIcon::Default => "default",
                                        },
                                    )
                                    .as_ref(),
                                ),
                                None => gdk_window.set_cursor(
                                    Cursor::for_display(&display, CursorType::BlankCursor).as_ref(),
                                ),
                            }
                        };
                    }
                    WindowRequest::CursorPosition((x, y)) => {
                        if let Some(cursor) = window
                            .display()
                            .default_seat()
                            .and_then(|seat| seat.pointer())
                        {
                            if let Some(screen) = GtkWindowExt::screen(&window) {
                                cursor.warp(&screen, x, y);
                            }
                        }
                    }
                    WindowRequest::CursorIgnoreEvents(ignore) => {
                        if ignore {
                            let empty_region =
                                Region::create_rectangle(&RectangleInt::new(0, 0, 1, 1));
                            window.window().unwrap().input_shape_combine_region(
                                &empty_region,
                                0,
                                0,
                            );
                        } else {
                            window.input_shape_combine_region(None)
                        };
                    }
                    // WindowRequest::ProgressBarState(_) => unreachable!(),
                    WindowRequest::WireUpEvents {
                        transparent,
                    } => {
                        window.add_events(
                            EventMask::POINTER_MOTION_MASK
                                | EventMask::BUTTON1_MOTION_MASK
                                | EventMask::BUTTON_PRESS_MASK
                                | EventMask::TOUCH_MASK
                                | EventMask::STRUCTURE_MASK
                                | EventMask::FOCUS_CHANGE_MASK
                                | EventMask::SCROLL_MASK,
                        );

                        // Allow resizing unmaximized borderless window
                        window.connect_motion_notify_event(|window, event| {
                            if !window.is_decorated()
                                && window.is_resizable()
                                && !window.is_maximized()
                            {
                                if let Some(window) = window.window() {
                                    let (cx, cy) = event.root();
                                    let edge = hit_test(&window, cx, cy);
                                    window.set_cursor(
                                        Cursor::from_name(
                                            &window.display(),
                                            match edge {
                                                WindowEdge::North => "n-resize",
                                                WindowEdge::South => "s-resize",
                                                WindowEdge::East => "e-resize",
                                                WindowEdge::West => "w-resize",
                                                WindowEdge::NorthWest => "nw-resize",
                                                WindowEdge::NorthEast => "ne-resize",
                                                WindowEdge::SouthEast => "se-resize",
                                                WindowEdge::SouthWest => "sw-resize",
                                                _ => "default",
                                            },
                                        )
                                        .as_ref(),
                                    );
                                }
                            }
                            glib::Propagation::Proceed
                        });
                        window.connect_button_press_event(|window, event| {
                            if !window.is_decorated()
                                && window.is_resizable()
                                && event.button() == 1
                            {
                                if let Some(window) = window.window() {
                                    let (cx, cy) = event.root();
                                    let result = hit_test(&window, cx, cy);

                                    // Ignore the `__Unknown` variant so the window receives the click correctly if it is not on the edges.
                                    match result {
                                        WindowEdge::__Unknown(_) => (),
                                        _ => {
                                            // FIXME: calling `window.begin_resize_drag` uses the default cursor, it should show a resizing cursor instead
                                            window.begin_resize_drag(
                                                result,
                                                1,
                                                cx as i32,
                                                cy as i32,
                                                event.time(),
                                            )
                                        }
                                    }
                                }
                            }

                            glib::Propagation::Proceed
                        });
                        window.connect_touch_event(|window, event| {
                            if !window.is_decorated() && window.is_resizable() {
                                if let Some(window) = window.window() {
                                    if let Some((cx, cy)) = event.root_coords() {
                                        if let Some(device) = event.device() {
                                            let result = hit_test(&window, cx, cy);

                                            // Ignore the `__Unknown` variant so the window receives the click correctly if it is not on the edges.
                                            match result {
                                                WindowEdge::__Unknown(_) => (),
                                                _ => window.begin_resize_drag_for_device(
                                                    result,
                                                    &device,
                                                    0,
                                                    cx as i32,
                                                    cy as i32,
                                                    event.time(),
                                                ),
                                            }
                                        }
                                    }
                                }
                            }

                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_delete_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::CloseRequested,
                            }) {
                                log::warn!(
                                    "Failed to send window close event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Stop
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_configure_event(move |window, event| {
                            let scale_factor = window.scale_factor();

                            let (x, y) = event.position();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::Moved(
                                    LogicalPosition::new(x, y).to_physical(scale_factor as f64),
                                ),
                            }) {
                                log::warn!(
                                    "Failed to send window moved event to event channel: {}",
                                    e
                                );
                            }

                            let (w, h) = event.size();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::Resized(
                                    LogicalSize::new(w, h).to_physical(scale_factor as f64),
                                ),
                            }) {
                                log::warn!(
                                    "Failed to send window resized event to event channel: {}",
                                    e
                                );
                            }
                            false
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_focus_in_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::Focused(true),
                            }) {
                                log::warn!(
                                    "Failed to send window focus-in event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_focus_out_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::Focused(false),
                            }) {
                                log::warn!(
                                    "Failed to send window focus-out event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_destroy(move |_| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::Destroyed,
                            }) {
                                log::warn!(
                                    "Failed to send window destroyed event to event channel: {}",
                                    e
                                );
                            }
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_enter_notify_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::CursorEntered {
                                    device_id: DEVICE_ID,
                                },
                            }) {
                                log::warn!(
                                    "Failed to send cursor entered event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_motion_notify_event(move |window, motion| {
                            if let Some(cursor) = motion.device() {
                              let scale_factor = window.scale_factor();
                              let (_, x, y) = cursor.window_at_position();
                              if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::CursorMoved {
                                  position: LogicalPosition::new(x, y).to_physical(scale_factor as f64),
                                  device_id: DEVICE_ID,
                                  // this field is depracted so it is fine to pass empty state
                                  modifiers: ModifiersState::empty(),
                                },
                              }) {
                                log::warn!("Failed to send cursor moved event to event channel: {}", e);
                              }
                          }
                          glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_leave_notify_event(move |_, _| {
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::CursorLeft {
                                    device_id: DEVICE_ID,
                                },
                            }) {
                                log::warn!(
                                    "Failed to send cursor left event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_button_press_event(move |_, event| {
                            let button = event.button();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::MouseInput {
                                    button: match button {
                                        1 => MouseButton::Left,
                                        2 => MouseButton::Middle,
                                        3 => MouseButton::Right,
                                        _ => MouseButton::Other(button as u16),
                                    },
                                    state: ElementState::Pressed,
                                    device_id: DEVICE_ID,
                                    // this field is depracted so it is fine to pass empty state
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!(
                                    "Failed to send mouse input preseed event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_button_release_event(move |_, event| {
                            let button = event.button();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::MouseInput {
                                    button: match button {
                                        1 => MouseButton::Left,
                                        2 => MouseButton::Middle,
                                        3 => MouseButton::Right,
                                        _ => MouseButton::Other(button as u16),
                                    },
                                    state: ElementState::Released,
                                    device_id: DEVICE_ID,
                                    // this field is depracted so it is fine to pass empty state
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!(
                                    "Failed to send mouse input released event to event channel: {}",
                                    e
                                );
                            }
                            glib::Propagation::Proceed
                        });

                        let tx_clone = event_tx.clone();
                        window.connect_scroll_event(move |_, event| {
                            let (x, y) = event.delta();
                            if let Err(e) = tx_clone.send(Event::WindowEvent {
                                window_id: RootWindowId(id),
                                event: WindowEvent::MouseWheel {
                                    device_id: DEVICE_ID,
                                    delta: MouseScrollDelta::LineDelta(-x as f32, -y as f32),
                                    phase: match event.direction() {
                                        ScrollDirection::Smooth => TouchPhase::Moved,
                                        _ => TouchPhase::Ended,
                                    },
                                    modifiers: ModifiersState::empty(),
                                },
                            }) {
                                log::warn!("Failed to send scroll event to event channel: {}", e);
                            }
                            glib::Propagation::Proceed
                        });

                        // TODO Follwong WindowEvents are missing see #2 for mor info.
                        // - Touch
                        // - TouchpadMagnify
                        // -  TouchpadRotate
                        // -  TouchpadPressure
                        // -  SmartMagnify
                        // -  ReceivedCharacter
                        // -  Ime
                        // - ScaleFactorChanged
                        // - DroppedFile
                        // - HoveredFile
                        // - HoveredFileCancelled
                        // - ThemeChanged
                        // - AxisMotion
                        // - Occluded

                        let tx_clone = event_tx.clone();
                        let modifiers = AtomicU32::new(ModifiersState::empty().bits());
                        let keyboard_handler =
                            Rc::new(move |event_key: EventKey, element_state| {
                                // if we have a modifier lets send it
                                let new_mods = keyboard::get_modifiers(&event_key);
                                    if new_mods.bits() != modifiers.load(Ordering::Relaxed) {
                                    modifiers.store(new_mods.bits(), Ordering::Relaxed);
                                    if let Err(e) = tx_clone.send(Event::WindowEvent {
                                        window_id: RootWindowId(id),
                                        event: WindowEvent::ModifiersChanged(new_mods),
                                    }) {
                                        log::warn!("Failed to send modifiers changed event to event channel: {}",e);
                                    }
                                }


                                let virtual_key = keyboard::gdk_key_to_virtual_key(event_key.keyval());
                                #[allow(deprecated)]
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: RootWindowId(id),
                                    event: WindowEvent::KeyboardInput {
                                        device_id: DEVICE_ID,
                                        input: KeyboardInput {
                                            scancode: event_key.scancode() as u32,
                                            state: element_state,
                                            virtual_keycode: virtual_key,
                                            modifiers: new_mods,
                                        },
                                        is_synthetic: false,
                                    },
                                }) {
                                    log::warn!(
                                        "Failed to send keyboard event to event channel: {}",
                                        e
                                    );
                                }

                                glib::ControlFlow::Continue
                            });

                            //     let tx_clone = event_tx.clone();
                            //     // TODO Add actual IME from system
                            //     let ime = gtk::IMContextSimple::default();
                            //     ime.set_client_window(window.window().as_ref());
                            //     ime.focus_in();
                            //     ime.connect_commit(move |_, s| {
                            // let c = s.chars().collect::<Vec<char>>();
                            //         if let Err(e) = tx_clone.send(Event::WindowEvent {
                            //             window_id: RootWindowId(id),
                            //             event: WindowEvent::ReceivedCharacter(c[0]),
                            //         }) {
                            //             log::warn!(
                            //                 "Failed to send received IME text event to event channel: {}",
                            //                 e
                            //             );
                            //         }
                            //     });

                            let handler = keyboard_handler.clone();
                            window.connect_key_press_event(move |_, event_key| {
                                handler(event_key.to_owned(), ElementState::Pressed);
                                // ime.filter_keypress(event_key);

                                glib::Propagation::Proceed
                            });

                            let handler = keyboard_handler.clone();
                            window.connect_key_release_event(move |_, event_key| {
                                handler(event_key.to_owned(), ElementState::Released);
                                glib::Propagation::Proceed
                            });

                        let tx_clone = event_tx.clone();
                        window.connect_window_state_event(move |window, event| {
                            let state = event.changed_mask();
                            if state.contains(WindowState::ICONIFIED)
                                || state.contains(WindowState::MAXIMIZED)
                            {
                                let scale_factor = window.scale_factor();

                                let (x, y) = window.position();
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: RootWindowId(id),
                                    event: WindowEvent::Moved(
                                        LogicalPosition::new(x, y).to_physical(scale_factor as f64),
                                    ),
                                }) {
                                    log::warn!(
                                        "Failed to send window moved event to event channel: {}",
                                        e
                                    );
                                }

                                let (w, h) = window.size();
                                if let Err(e) = tx_clone.send(Event::WindowEvent {
                                    window_id: RootWindowId(id),
                                    event: WindowEvent::Resized(
                                        LogicalSize::new(w, h).to_physical(scale_factor as f64),
                                    ),
                                }) {
                                    log::warn!(
                                        "Failed to send window resized event to event channel: {}",
                                        e
                                    );
                                }
                            }
                            glib::Propagation::Proceed
                        });

                        // Receive draw events of the window.
                        let draw_clone = draw_tx.clone();
                        window.connect_draw(move |_, cr| {
                            if let Err(e) = draw_clone.send(id) {
                                log::warn!("Failed to send redraw event to event channel: {}", e);
                            }

                            if transparent.load(Ordering::Relaxed) {
                                cr.set_source_rgba(0., 0., 0., 0.);
                                cr.set_operator(cairo::Operator::Source);
                                let _ = cr.paint();
                                cr.set_operator(cairo::Operator::Over);
                            }

                            glib::Propagation::Proceed
                        });
                    }
                }
            }
            glib::ControlFlow::Continue
        });

        // Create event loop itself.
        Self {
            window_target: RootELW {
                p: window_target,
                _marker: std::marker::PhantomData,
            },
            user_event_tx,
            events: event_rx,
            draws: draw_rx,
        }
    }
    /// Creates an `EventLoopProxy` that can be used to dispatch user events to the main event loop.
    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy {
            user_event_tx: self.user_event_tx.clone(),
        }
    }

    #[inline]
    pub fn run<F>(mut self, callback: F) -> !
    where
        F: 'static + FnMut(crate::event::Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let exit_code = self.run_return(callback);
        process::exit(exit_code)
    }

    /// This is the core event loop logic. It basically loops on `gtk_main_iteration` and processes one
    /// event along with that iteration. Depends on current control flow and what it should do, an
    /// event state is defined. The whole state flow chart runs like following:
    ///
    /// ```ignore
    ///                                   Poll/Wait/WaitUntil
    ///       +-------------------------------------------------------------------------+
    ///       |                                                                         |
    ///       |                   Receiving event from event channel                    |   Receiving event from draw channel
    ///       |                               +-------+                                 |   +---+
    ///       v                               v       |                                 |   v   |
    /// +----------+  Poll/Wait/WaitUntil   +------------+  Poll/Wait/WaitUntil   +-----------+ |
    /// | NewStart | ---------------------> | EventQueue | ---------------------> | DrawQueue | |
    /// +----------+                        +------------+                        +-----------+ |
    ///       |ExitWithCode                        |ExitWithCode            ExitWithCode|   |   |
    ///       +------------------------------------+------------------------------------+   +---+
    ///                                            |
    ///                                            v
    ///                                    +---------------+
    ///                                    | LoopDestroyed |
    ///                                    +---------------+
    /// ```
    ///
    /// There are a dew notibale event will sent to callback when state is transisted:
    /// - On any state moves to `LoopDestroyed`, a `LoopDestroyed` event is sent.
    /// - On `NewStart` to `EventQueue`, a `NewEvents` with corresponding `StartCause` depends on
    /// current control flow is sent.
    /// - On `EventQueue` to `DrawQueue`, a `MainEventsCleared` event is sent.
    /// - On `DrawQueue` back to `NewStart`, a `RedrawEventsCleared` event is sent.
    pub(crate) fn run_return<F>(&mut self, mut callback: F) -> i32
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        enum EventState {
            NewStart,
            EventQueue,
            DrawQueue,
        }

        let context = MainContext::default();
        context
            .with_thread_default(|| {
                let mut control_flow = ControlFlow::default();
                let window_target = &self.window_target;
                let events = &self.events;
                let draws = &self.draws;

                window_target.p.app.activate();

                let mut state = EventState::NewStart;
                let exit_code = loop {
                    let mut blocking = false;
                    match state {
                        EventState::NewStart => match control_flow {
                            ControlFlow::ExitWithCode(code) => {
                                callback(Event::LoopDestroyed, window_target, &mut control_flow);
                                break code;
                            }
                            ControlFlow::Wait => {
                                if !events.is_empty() {
                                    callback(
                                        Event::NewEvents(StartCause::WaitCancelled {
                                            start: Instant::now(),
                                            requested_resume: None,
                                        }),
                                        window_target,
                                        &mut control_flow,
                                    );
                                    state = EventState::EventQueue;
                                } else {
                                    blocking = true;
                                }
                            }
                            ControlFlow::WaitUntil(requested_resume) => {
                                let start = Instant::now();
                                if start >= requested_resume {
                                    callback(
                                        Event::NewEvents(StartCause::ResumeTimeReached {
                                            start,
                                            requested_resume,
                                        }),
                                        window_target,
                                        &mut control_flow,
                                    );
                                    state = EventState::EventQueue;
                                } else if !events.is_empty() {
                                    callback(
                                        Event::NewEvents(StartCause::WaitCancelled {
                                            start,
                                            requested_resume: Some(requested_resume),
                                        }),
                                        window_target,
                                        &mut control_flow,
                                    );
                                    state = EventState::EventQueue;
                                } else {
                                    blocking = true;
                                }
                            }
                            _ => {
                                callback(
                                    Event::NewEvents(StartCause::Poll),
                                    window_target,
                                    &mut control_flow,
                                );
                                state = EventState::EventQueue;
                            }
                        },
                        EventState::EventQueue => match control_flow {
                            ControlFlow::ExitWithCode(code) => {
                                callback(Event::LoopDestroyed, window_target, &mut control_flow);
                                break code;
                            }
                            _ => match events.try_recv() {
                                Ok(event) => match event {
                                    Event::LoopDestroyed => {
                                        control_flow = ControlFlow::ExitWithCode(1)
                                    }
                                    _ => callback(event, window_target, &mut control_flow),
                                },
                                Err(_) => {
                                    callback(
                                        Event::MainEventsCleared,
                                        window_target,
                                        &mut control_flow,
                                    );
                                    state = EventState::DrawQueue;
                                }
                            },
                        },
                        EventState::DrawQueue => match control_flow {
                            ControlFlow::ExitWithCode(code) => {
                                callback(Event::LoopDestroyed, window_target, &mut control_flow);
                                break code;
                            }
                            _ => {
                                if let Ok(id) = draws.try_recv() {
                                    callback(
                                        Event::RedrawRequested(RootWindowId(id)),
                                        window_target,
                                        &mut control_flow,
                                    );
                                }
                                callback(
                                    Event::RedrawEventsCleared,
                                    window_target,
                                    &mut control_flow,
                                );
                                state = EventState::NewStart;
                            }
                        },
                    }
                    gtk::main_iteration_do(blocking);
                };
                exit_code
            })
            .unwrap_or(1)
    }

    pub fn window_target(&self) -> &crate::event_loop::EventLoopWindowTarget<T> {
        &self.window_target
    }
}

impl<T: 'static> EventLoopProxy<T> {
    pub fn send_event(&self, event: T) -> Result<(), EventLoopClosed<T>> {
        self.user_event_tx
            .send(Event::UserEvent(event))
            .map_err(|SendError(event)| {
                if let Event::UserEvent(error) = event {
                    EventLoopClosed(error)
                } else {
                    unreachable!();
                }
            })?;

        let context = MainContext::default();
        context.wakeup();

        Ok(())
    }
}

#[derive(Clone)]
pub struct EventLoopWindowTarget<T> {
    /// Gdk display
    pub(crate) display: gdk::Display,
    /// Gtk application
    pub(crate) app: gtk::Application,
    /// Window Ids of the application
    pub(crate) windows: Rc<RefCell<HashSet<WindowId>>>,
    /// Window requests sender
    pub(crate) window_requests_tx: glib::Sender<(WindowId, WindowRequest)>,
    /// Draw event sender
    pub(crate) draw_tx: crossbeam_channel::Sender<WindowId>,
    _marker: std::marker::PhantomData<T>,
}
impl<T> EventLoopWindowTarget<T> {
    #[inline]
    pub fn is_wayland(&self) -> bool {
        self.display.backend().is_wayland()
    }

    #[inline]
    pub fn available_monitors(&self) -> VecDeque<MonitorHandle> {
        let mut handles = VecDeque::new();
        let display = &self.display;
        let numbers = display.n_monitors();

        for i in 0..numbers {
            let monitor = MonitorHandle::new(display, i);
            handles.push_back(monitor);
        }

        handles
    }

    #[inline]
    pub fn primary_monitor(&self) -> Option<MonitorHandle> {
        let monitor = self.display.primary_monitor();
        monitor.map(|monitor| MonitorHandle { monitor })
    }

    #[inline]
    pub fn set_device_event_filter(&self, _filter: DeviceEventFilter) {
        // TODO implement this
    }

    pub fn raw_display_handle(&self) -> raw_window_handle::RawDisplayHandle {
        if self.is_wayland() {
            let dummy_ptr: *mut c_void = 1_usize as *mut c_void;
            let dummy_nonnull = unsafe { NonNull::new_unchecked(dummy_ptr) };
            let mut display_handle = WaylandDisplayHandle::new(dummy_nonnull);
            display_handle.display = unsafe {
                let not_null_display = gdk_wayland_sys::gdk_wayland_display_get_wl_display(
                    self.display.as_ptr() as *mut _,
                );
                if let Some(display) = NonNull::new(not_null_display) {
                    display
                } else {
                    panic!("")
                }
            };
            RawDisplayHandle::Wayland(display_handle)
        } else {
            let dummy_ptr: *mut c_void = 1_usize as *mut c_void;
            let dummy_nonnull = unsafe { NonNull::new_unchecked(dummy_ptr) };
            let mut display_handle = XlibDisplayHandle::new(Some(dummy_nonnull), 0);
            unsafe {
                if let Ok(xlib) = x11_dl::xlib::Xlib::open() {
                    let display = (xlib.XOpenDisplay)(std::ptr::null());
                    if let Some(non_null_display) = NonNull::new(display as *mut c_void) {
                        display_handle.display = Some(non_null_display);
                        display_handle.screen = (xlib.XDefaultScreen)(display) as _;
                    }
                }
            }
            RawDisplayHandle::Xlib(display_handle)
        }
    }
}
