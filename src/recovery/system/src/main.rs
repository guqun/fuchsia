// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "http_setup_server")]
mod button;
#[cfg(feature = "debug_console")]
mod console;
mod fdr;
mod font;
#[cfg(feature = "http_setup_server")]
mod keyboard;
#[cfg(feature = "http_setup_server")]
mod keys;
#[cfg(feature = "http_setup_server")]
mod ota;
mod proxy_view_assistant;
#[cfg(feature = "http_setup_server")]
mod setup;
#[cfg(feature = "http_setup_server")]
mod storage;

use anyhow::{format_err, Error};
use argh::FromArgs;
use carnelian::{
    app::Config,
    color::Color,
    drawing::{path_for_circle, DisplayRotation, FontFace},
    input, make_message,
    render::{rive::load_rive, BlendMode, Context as RenderContext, Fill, FillRule, Raster, Style},
    scene::{
        facets::{
            RasterFacet, RiveFacet, TextFacetOptions, TextHorizontalAlignment,
            TextVerticalAlignment,
        },
        layout::{
            CrossAxisAlignment, Flex, FlexOptions, MainAxisAlignment, MainAxisSize, Stack,
            StackOptions,
        },
        scene::{Scene, SceneBuilder},
    },
    App, AppAssistant, AppAssistantPtr, AppSender, AssistantCreatorFunc, Coord, LocalBoxFuture,
    MessageTarget, Point, Size, ViewAssistant, ViewAssistantContext, ViewAssistantPtr, ViewKey,
};
use euclid::size2;
use fdr::{FactoryResetState, ResetEvent};
use fidl_fuchsia_input_report::ConsumerControlButton;
use fidl_fuchsia_recovery::FactoryResetMarker;
use fidl_fuchsia_recovery_policy::FactoryResetMarker as FactoryResetPolicyMarker;
#[cfg(feature = "http_setup_server")]
use fuchsia_async::DurationExt;
use fuchsia_async::{self as fasync, Task};
use fuchsia_component::client::connect_to_protocol;
use fuchsia_zircon::{Duration, Event};
use futures::StreamExt;
use rive_rs::{self as rive};
use std::borrow::{Borrow, Cow};
use std::path::Path;

const FACTORY_RESET_TIMER_IN_SECONDS: u8 = 10;
const LOGO_IMAGE_PATH: &str = "/pkg/data/logo.riv";
const BG_COLOR: Color = Color::white();
const HEADING_COLOR: Color = Color::new();
const BODY_COLOR: Color = Color { r: 0x7e, g: 0x86, b: 0x8d, a: 0xff };
const COUNTDOWN_COLOR: Color = Color { r: 0x42, g: 0x85, b: 0xf4, a: 0xff };
#[cfg(feature = "http_setup_server")]
const BUTTON_BORDER: f32 = 7.0;
#[cfg(feature = "http_setup_server")]
// Space added to between rows can have any width.
const SPACE_WIDTH: f32 = 10.0;
#[cfg(feature = "http_setup_server")]
// Nice space for text below a button
const SPACE_HEIGHT: f32 = 10.0;

#[cfg(feature = "http_setup_server")]
use crate::{
    button::{Button, ButtonMessages},
    keyboard::{KeyboardMessages, KeyboardViewAssistant},
    setup::SetupEvent,
};

#[cfg(feature = "debug_console")]
use crate::console::{ConsoleMessages, ConsoleViewAssistant};

#[cfg(feature = "http_setup_server")]
use crate::proxy_view_assistant::ProxyMessages;
use crate::proxy_view_assistant::ProxyViewAssistant;

fn raster_for_circle(center: Point, radius: Coord, render_context: &mut RenderContext) -> Raster {
    let path = path_for_circle(center, radius, render_context);
    let mut raster_builder = render_context.raster_builder().expect("raster_builder");
    raster_builder.add(&path, None);
    raster_builder.build()
}

/// FDR
#[derive(Debug, FromArgs)]
#[argh(name = "recovery")]
struct Args {
    /// rotate
    #[argh(option)]
    rotation: Option<DisplayRotation>,
}

enum RecoveryMessages {
    #[cfg(feature = "http_setup_server")]
    EventReceived,
    #[cfg(feature = "http_setup_server")]
    StartingOta,
    #[cfg(feature = "http_setup_server")]
    OtaFinished {
        result: Result<(), Error>,
    },
    PolicyResult(usize, bool),
    ResetMessage(FactoryResetState),
    CountdownTick(u8),
    ResetFailed,
}

#[derive(Clone, PartialEq)]
#[cfg(feature = "http_setup_server")]
enum WiFiMessages {
    Connecting,
    Connected(bool),
}

const RECOVERY_MODE_HEADLINE: &'static str = "Recovery mode";
const RECOVERY_MODE_BODY: &'static str = "\nPress and hold both volume keys to factory reset.";

const COUNTDOWN_MODE_HEADLINE: &'static str = "Factory reset device";
const COUNTDOWN_MODE_BODY: &'static str =
    "\nContinue holding the keys to the end of the countdown. \
This will wipe all of your data from this device and reset it to factory settings.";

#[cfg(feature = "http_setup_server")]
const WIFI_SSID: &'static str = "WiFi SSID";
#[cfg(feature = "http_setup_server")]
const WIFI_PASSWORD: &'static str = "WiFi Password";
#[cfg(feature = "http_setup_server")]
const WIFI_CONNECT: &'static str = "WiFi Connect";

const PATH_TO_FDR_RESTRICTION_CONFIG: &'static str = "/config/data/check_fdr_restriction.json";

/// An enum to track whether fdr is restricted or not.
#[derive(Copy, Clone)]
enum FdrRestriction {
    /// Fdr is not restricted and can proceed without any additional checks.
    NotRestricted,
    /// Fdr is possibly restricted. The policy should be checked when attempting
    /// factory device reset.
    Restricted { fdr_initially_enabled: bool },
}

impl FdrRestriction {
    fn is_initially_enabled(&self) -> bool {
        match self {
            FdrRestriction::NotRestricted => true,
            FdrRestriction::Restricted { fdr_initially_enabled } => *fdr_initially_enabled,
        }
    }
}

struct RecoveryAppAssistant {
    app_sender: AppSender,
    display_rotation: DisplayRotation,
    fdr_restriction: FdrRestriction,
}

impl RecoveryAppAssistant {
    pub fn new(app_sender: &AppSender, fdr_restriction: FdrRestriction) -> Self {
        let args: Args = argh::from_env();

        Self {
            app_sender: app_sender.clone(),
            display_rotation: args.rotation.unwrap_or(DisplayRotation::Deg90),
            fdr_restriction,
        }
    }
}

impl AppAssistant for RecoveryAppAssistant {
    fn setup(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn create_view_assistant(&mut self, view_key: ViewKey) -> Result<ViewAssistantPtr, Error> {
        let body = get_recovery_body(self.fdr_restriction.is_initially_enabled());
        let file = load_rive(LOGO_IMAGE_PATH).ok();

        #[cfg(feature = "debug_console")]
        let console_view_assistant_ptr = Box::new(ConsoleViewAssistant::new()?);

        let view_assistant_ptr = Box::new(RecoveryViewAssistant::new(
            &self.app_sender,
            view_key,
            file,
            RECOVERY_MODE_HEADLINE,
            body.map(Into::into),
            self.fdr_restriction,
        )?);

        // ProxyView is a root view that conditionally displays the top View
        // from a stack (initialized with "RecoveryView"), or the Console.
        let proxy_ptr = Box::new(ProxyViewAssistant::new(
            #[cfg(feature = "debug_console")]
            console_view_assistant_ptr,
            view_assistant_ptr,
        )?);
        Ok(proxy_ptr)
    }

    fn filter_config(&mut self, config: &mut Config) {
        config.display_rotation = self.display_rotation;
    }
}
#[cfg(feature = "http_setup_server")]
struct RenderResources {
    scene: Scene,
    #[cfg(feature = "http_setup_server")]
    buttons: Vec<Button>,
}

#[cfg(not(feature = "http_setup_server"))]
struct RenderResources {
    scene: Scene,
}

impl RenderResources {
    fn new(
        render_context: &mut RenderContext,
        file: &Option<rive::File>,
        target_size: Size,
        heading: &str,
        body: Option<&str>,
        countdown_ticks: u8,
        #[cfg(feature = "http_setup_server")] wifi_ssid: &Option<String>,
        #[cfg(feature = "http_setup_server")] wifi_password: &Option<String>,
        #[cfg(feature = "http_setup_server")] wifi_connected: &WiFiMessages,
        face: &FontFace,
        is_counting_down: bool,
    ) -> Self {
        let min_dimension = target_size.width.min(target_size.height);
        let logo_edge = min_dimension * 0.24;
        let text_size = min_dimension / 10.0;
        let body_text_size = min_dimension / 18.0;
        #[cfg(feature = "http_setup_server")]
        let button_text_size = min_dimension / 14.0;
        let countdown_text_size = min_dimension / 6.0;
        #[cfg(feature = "http_setup_server")]
        let mut buttons: Vec<Button> = Vec::new();

        let mut builder = SceneBuilder::new().background_color(BG_COLOR).round_scene_corners(true);
        builder.group().column().max_size().main_align(MainAxisAlignment::Start).contents(
            |builder| {
                let logo_size: Size = size2(logo_edge, logo_edge);
                builder.space(size2(min_dimension, 50.0));

                if is_counting_down {
                    // Centre the circle and countdown in the screen
                    builder.start_group(
                        "circle_and_countdown_row",
                        Flex::with_options_ptr(FlexOptions::row(
                            MainAxisSize::Max,
                            MainAxisAlignment::Center,
                            CrossAxisAlignment::End,
                        )),
                    );

                    // Stack the circle and text
                    builder.start_group(
                        "circle_stack",
                        Stack::with_options_ptr(StackOptions {
                            expand: false,
                            alignment: carnelian::scene::layout::Alignment::center(),
                        }),
                    );

                    // Place countdown text
                    builder.text(
                        face.clone(),
                        &format!("{}", countdown_ticks),
                        countdown_text_size,
                        Point::zero(),
                        TextFacetOptions {
                            color: Color::white(),
                            horizontal_alignment: TextHorizontalAlignment::Center,
                            vertical_alignment: TextVerticalAlignment::Bottom,
                            visual: true,
                            ..TextFacetOptions::default()
                        },
                    );

                    let circle = raster_for_circle(
                        Point::new(logo_edge / 2.0, logo_edge / 2.0),
                        logo_edge / 2.0,
                        render_context,
                    );
                    let circle_facet = RasterFacet::new(
                        circle,
                        Style {
                            fill_rule: FillRule::NonZero,
                            fill: Fill::Solid(COUNTDOWN_COLOR),
                            blend_mode: BlendMode::Over,
                        },
                        logo_size,
                    );
                    builder.facet(Box::new(circle_facet));

                    builder.end_group(); // circle_stack
                    builder.end_group(); // circle_and_countdown_row
                } else {
                    if let Some(file) = file {
                        // Centre the logo
                        builder.start_group(
                            "logo_row",
                            Flex::with_options_ptr(FlexOptions::row(
                                MainAxisSize::Max,
                                MainAxisAlignment::Center,
                                CrossAxisAlignment::End,
                            )),
                        );

                        let facet = RiveFacet::new_from_file(logo_size, &file, None)
                            .expect("facet_from_file");
                        builder.facet(Box::new(facet));
                        builder.end_group(); // logo_row
                    }
                }

                builder.space(size2(min_dimension, 50.0));
                builder.text(
                    face.clone(),
                    &heading,
                    text_size,
                    Point::zero(),
                    TextFacetOptions {
                        horizontal_alignment: TextHorizontalAlignment::Center,
                        color: HEADING_COLOR,
                        ..TextFacetOptions::default()
                    },
                );

                let wrap_width = target_size.width * 0.8;

                if let Some(body) = body {
                    builder.text(
                        face.clone(),
                        &body,
                        body_text_size,
                        Point::zero(),
                        TextFacetOptions {
                            horizontal_alignment: TextHorizontalAlignment::Left,
                            color: BODY_COLOR,
                            max_width: Some(wrap_width),
                            ..TextFacetOptions::default()
                        },
                    );
                }

                // Add the WiFi connection buttons.
                #[cfg(feature = "http_setup_server")]
                if !is_counting_down {
                    builder.space(size2(min_dimension, 50.0));
                    builder.start_group(
                        "button_row",
                        Flex::with_options_ptr(FlexOptions::row(
                            MainAxisSize::Max,
                            MainAxisAlignment::SpaceEvenly,
                            CrossAxisAlignment::End,
                        )),
                    );

                    let button_text = WIFI_SSID;
                    let info_text = if let Some(wifi_ssid) = wifi_ssid.as_ref() {
                        wifi_ssid
                    } else {
                        "<No Network Name>"
                    };
                    RenderResources::build_button_group(
                        face,
                        body_text_size,
                        button_text_size,
                        &mut buttons,
                        builder,
                        wrap_width,
                        &button_text,
                        info_text,
                    );

                    let button_text = WIFI_PASSWORD;
                    let info_text =
                        if wifi_password.is_some() && !wifi_password.as_ref().unwrap().is_empty() {
                            "********"
                        } else {
                            "<No Password>"
                        };
                    RenderResources::build_button_group(
                        face,
                        body_text_size,
                        button_text_size,
                        &mut buttons,
                        builder,
                        wrap_width,
                        &button_text,
                        info_text,
                    );

                    let button_text = WIFI_CONNECT;
                    let info_text = match wifi_connected {
                        WiFiMessages::Connecting => "Connecting",
                        WiFiMessages::Connected(connected) => {
                            if *connected {
                                "Connected"
                            } else {
                                "Not Connected"
                            }
                        }
                    };
                    RenderResources::build_button_group(
                        face,
                        body_text_size,
                        button_text_size,
                        &mut buttons,
                        builder,
                        wrap_width,
                        &button_text,
                        info_text,
                    );

                    // End row button_row
                    builder.end_group();
                }
            },
        );

        let mut scene = builder.build();
        scene.layout(target_size);
        #[cfg(feature = "http_setup_server")]
        {
            if buttons.len() >= 3 {
                buttons[0].set_focused(&mut scene, true);
                buttons[1].set_focused(&mut scene, true);
                buttons[2].set_focused(
                    &mut scene,
                    wifi_ssid.is_some() && wifi_connected == &WiFiMessages::Connected(false),
                );
            }
            Self { scene, buttons }
        }

        #[cfg(not(feature = "http_setup_server"))]
        Self { scene }
    }

    #[cfg(feature = "http_setup_server")]
    fn build_button_group(
        face: &FontFace,
        body_text_size: f32,
        button_text_size: f32,
        buttons: &mut Vec<Button>,
        builder: &mut SceneBuilder,
        wrap_width: f32,
        button_text: &&str,
        info_text: &str,
    ) {
        builder.start_group(
            &("button_group_".to_owned() + button_text),
            Flex::with_options_ptr(FlexOptions::column(
                MainAxisSize::Min,
                MainAxisAlignment::SpaceEvenly,
                CrossAxisAlignment::Center,
            )),
        );
        buttons.push(
            Button::new(button_text, button_text_size, BUTTON_BORDER, builder).expect(button_text),
        );
        builder.space(size2(SPACE_WIDTH, SPACE_HEIGHT));
        builder.text(
            face.clone(),
            info_text,
            body_text_size,
            Point::zero(),
            TextFacetOptions {
                horizontal_alignment: TextHorizontalAlignment::Left,
                color: BODY_COLOR,
                max_width: Some(wrap_width),
                ..TextFacetOptions::default()
            },
        );
        // End column button_group_password
        builder.end_group();
    }
}

struct RecoveryViewAssistant {
    face: FontFace,
    heading: &'static str,
    body: Option<Cow<'static, str>>,
    fdr_restriction: FdrRestriction,
    reset_state_machine: fdr::FactoryResetStateMachine,
    app_sender: AppSender,
    view_key: ViewKey,
    file: Option<rive::File>,
    countdown_task: Option<Task<()>>,
    countdown_ticks: u8,
    render_resources: Option<RenderResources>,
    #[cfg(feature = "http_setup_server")]
    wifi_ssid: Option<String>,
    #[cfg(feature = "http_setup_server")]
    wifi_password: Option<String>,
    #[cfg(feature = "http_setup_server")]
    connected: WiFiMessages,
}

impl RecoveryViewAssistant {
    fn new(
        app_sender: &AppSender,
        view_key: ViewKey,
        file: Option<rive::File>,
        heading: &'static str,
        body: Option<Cow<'static, str>>,
        fdr_restriction: FdrRestriction,
    ) -> Result<RecoveryViewAssistant, Error> {
        RecoveryViewAssistant::setup(app_sender, view_key)?;

        let face = font::load_default_font_face()?;
        Ok(RecoveryViewAssistant {
            face,
            heading,
            body,
            fdr_restriction,
            reset_state_machine: fdr::FactoryResetStateMachine::new(),
            app_sender: app_sender.clone(),
            view_key,
            file,
            countdown_task: None,
            countdown_ticks: FACTORY_RESET_TIMER_IN_SECONDS,
            render_resources: None,
            #[cfg(feature = "http_setup_server")]
            wifi_ssid: None,
            #[cfg(feature = "http_setup_server")]
            wifi_password: None,
            #[cfg(feature = "http_setup_server")]
            connected: WiFiMessages::Connected(false),
        })
    }

    #[cfg(not(feature = "http_setup_server"))]
    fn setup(_: &AppSender, _: ViewKey) -> Result<(), Error> {
        Ok(())
    }

    #[cfg(feature = "http_setup_server")]
    fn setup(app_sender: &AppSender, view_key: ViewKey) -> Result<(), Error> {
        use futures::FutureExt as _;

        // TODO: it should be possible to pass a handler function and avoid the need for message
        // passing, but AppSender eventually contains carnelian::FrameBufferPtr which is an alias
        // for Rc<_> which is !Send.
        let (sender, receiver) = async_channel::unbounded();
        fasync::Task::local(
            setup::start_server(move |event| async move { sender.send(event).await.unwrap() }).map(
                |result| {
                    result
                        .unwrap_or_else(|error| eprintln!("recovery: HTTP server error: {}", error))
                },
            ),
        )
        .detach();

        fasync::Task::local(
            receiver
                .fold(app_sender.clone(), move |local_app_sender, event| async move {
                    println!("recovery: received request");
                    match event {
                        SetupEvent::Root => local_app_sender.queue_message(
                            MessageTarget::View(view_key),
                            make_message(RecoveryMessages::EventReceived),
                        ),
                        SetupEvent::DevhostOta { cfg } => {
                            local_app_sender.queue_message(
                                MessageTarget::View(view_key),
                                make_message(RecoveryMessages::StartingOta),
                            );
                            let result = ota::run_devhost_ota(cfg).await;
                            local_app_sender.queue_message(
                                MessageTarget::View(view_key),
                                make_message(RecoveryMessages::OtaFinished { result }),
                            );
                        }
                    }
                    local_app_sender
                })
                .map(|_: AppSender| ()),
        )
        .detach();

        Ok(())
    }

    /// Checks whether fdr policy allows factory reset to be performed. If not, then it will not
    /// move forward with the reset. If it is, then it will forward the message to begin reset.
    async fn check_fdr_and_maybe_reset(view_key: ViewKey, app_sender: AppSender, check_id: usize) {
        let fdr_enabled = check_fdr_enabled().await.unwrap_or_else(|error| {
            eprintln!("recovery: Error occurred, but proceeding with reset: {:?}", error);
            true
        });
        app_sender.queue_message(
            MessageTarget::View(view_key),
            make_message(RecoveryMessages::PolicyResult(check_id, fdr_enabled)),
        );
    }

    async fn execute_reset(view_key: ViewKey, app_sender: AppSender) {
        let factory_reset_service = connect_to_protocol::<FactoryResetMarker>();
        let proxy = match factory_reset_service {
            Ok(marker) => marker.clone(),
            Err(error) => {
                app_sender.queue_message(
                    MessageTarget::View(view_key),
                    make_message(RecoveryMessages::ResetFailed),
                );
                panic!("Could not connect to factory_reset_service: {:?}", error);
            }
        };

        println!("recovery: Executing factory reset command");

        let res = proxy.reset().await;
        match res {
            Ok(_) => {}
            Err(error) => {
                app_sender.queue_message(
                    MessageTarget::View(view_key),
                    make_message(RecoveryMessages::ResetFailed),
                );
                eprintln!("recovery: Error occurred : {:?}", error);
            }
        };
    }

    fn handle_recovery_message(&mut self, message: &RecoveryMessages) {
        match message {
            #[cfg(feature = "http_setup_server")]
            RecoveryMessages::EventReceived => {
                self.body = Some("Got event".into());
            }
            #[cfg(feature = "http_setup_server")]
            RecoveryMessages::StartingOta => {
                self.body = Some("Starting OTA update".into());
            }
            #[cfg(feature = "http_setup_server")]
            RecoveryMessages::OtaFinished { result } => {
                if let Err(e) = result {
                    self.body = Some(format!("OTA failed: {:?}", e).into());
                } else {
                    self.body = Some("OTA succeeded".into());
                }
            }
            RecoveryMessages::PolicyResult(check_id, fdr_enabled) => {
                let state = self
                    .reset_state_machine
                    .handle_event(ResetEvent::AwaitPolicyResult(*check_id, *fdr_enabled));
                self.app_sender.queue_message(
                    MessageTarget::View(self.view_key),
                    make_message(RecoveryMessages::ResetMessage(state)),
                );
            }
            RecoveryMessages::ResetMessage(state) => {
                match state {
                    FactoryResetState::Waiting => {
                        self.heading = RECOVERY_MODE_HEADLINE;
                        self.body = get_recovery_body(self.fdr_restriction.is_initially_enabled())
                            .map(Into::into);
                        self.render_resources = None;
                        self.app_sender.request_render(self.view_key);
                    }
                    FactoryResetState::AwaitingPolicy(_) => {} // no-op
                    FactoryResetState::StartCountdown => {
                        let view_key = self.view_key;
                        let local_app_sender = self.app_sender.clone();

                        let mut counter = FACTORY_RESET_TIMER_IN_SECONDS;
                        local_app_sender.queue_message(
                            MessageTarget::View(view_key),
                            make_message(RecoveryMessages::CountdownTick(counter)),
                        );

                        // start the countdown timer
                        let f = async move {
                            let mut interval_timer =
                                fasync::Interval::new(Duration::from_seconds(1));
                            while let Some(()) = interval_timer.next().await {
                                counter -= 1;
                                local_app_sender.queue_message(
                                    MessageTarget::View(view_key),
                                    make_message(RecoveryMessages::CountdownTick(counter)),
                                );
                                if counter == 0 {
                                    break;
                                }
                            }
                        };
                        self.countdown_task = Some(fasync::Task::local(f));
                    }
                    FactoryResetState::CancelCountdown => {
                        self.countdown_task
                            .take()
                            .and_then(|task| Some(fasync::Task::local(task.cancel())));
                        let state =
                            self.reset_state_machine.handle_event(ResetEvent::CountdownCancelled);
                        assert_eq!(state, fdr::FactoryResetState::Waiting);
                        self.app_sender.queue_message(
                            MessageTarget::View(self.view_key),
                            make_message(RecoveryMessages::ResetMessage(state)),
                        );
                    }
                    FactoryResetState::ExecuteReset => {
                        let view_key = self.view_key;
                        let local_app_sender = self.app_sender.clone();
                        let f = async move {
                            RecoveryViewAssistant::execute_reset(view_key, local_app_sender).await;
                        };
                        fasync::Task::local(f).detach();
                    }
                };
            }
            RecoveryMessages::CountdownTick(count) => {
                self.heading = COUNTDOWN_MODE_HEADLINE;
                self.countdown_ticks = *count;
                if *count == 0 {
                    self.body = Some("Resetting device...".into());
                    let state =
                        self.reset_state_machine.handle_event(ResetEvent::CountdownFinished);
                    assert_eq!(state, FactoryResetState::ExecuteReset);
                    self.app_sender.queue_message(
                        MessageTarget::View(self.view_key),
                        make_message(RecoveryMessages::ResetMessage(state)),
                    );
                } else {
                    self.body = Some(COUNTDOWN_MODE_BODY.into());
                }
                self.render_resources = None;
                self.app_sender.request_render(self.view_key);
            }
            RecoveryMessages::ResetFailed => {
                self.heading = "Reset failed";
                self.body = Some("Please restart device to try again".into());
                self.render_resources = None;
                self.app_sender.request_render(self.view_key);
            }
        }
    }

    #[cfg(feature = "http_setup_server")]
    fn handle_keyboard_message(&mut self, message: &KeyboardMessages) {
        match message {
            KeyboardMessages::NoInput => {}
            KeyboardMessages::Result(WIFI_SSID, result) => {
                self.wifi_ssid = if result.len() == 0 { None } else { Some(result.clone()) };
                self.render_resources = None;
            }
            KeyboardMessages::Result(WIFI_PASSWORD, result) => {
                // Allow empty passwords
                self.wifi_password = Some(result.clone());
                self.render_resources = None;
            }
            KeyboardMessages::Result(_, _) => {}
        }
    }

    #[cfg(feature = "http_setup_server")]
    fn handle_button_message(&mut self, message: &ButtonMessages) {
        match message {
            ButtonMessages::Pressed(_, label) => {
                #[cfg(feature = "debug_console")]
                self.app_sender.queue_message(
                    MessageTarget::View(self.view_key),
                    make_message(ConsoleMessages::AddText(format!("Button pressed: {}", label))),
                );

                let mut entry_text = String::new();
                let mut field_text = "";
                match label.as_ref() {
                    WIFI_SSID => {
                        field_text = WIFI_SSID;
                        if let Some(ssid) = &self.wifi_ssid {
                            entry_text = ssid.clone();
                        }
                    }
                    WIFI_PASSWORD => {
                        field_text = WIFI_PASSWORD;
                        if let Some(password) = &self.wifi_password {
                            entry_text = password.clone();
                        }
                    }
                    WIFI_CONNECT => {
                        if let Some(ssid) = &self.wifi_ssid {
                            // Allow empty passwords
                            let mut password = "";
                            if let Some(wifi_password) = &self.wifi_password {
                                password = wifi_password;
                            };
                            let local_app_sender = self.app_sender.clone();
                            local_app_sender.queue_message(
                                MessageTarget::View(self.view_key),
                                make_message(WiFiMessages::Connecting),
                            );

                            let ssid = ssid.clone();
                            let password = password.to_string().clone();
                            let view_key = self.view_key.clone();
                            let f = async move {
                                let connected = connect_to_wifi(ssid, password).await;

                                if connected.is_err() {
                                    // Let the "Connecting" message stay there for a second so the
                                    // user can see that something was tried.
                                    let sleep_time = Duration::from_seconds(1);
                                    fuchsia_async::Timer::new(sleep_time.after_now()).await;
                                    println!(
                                        "Failed to connect: {}",
                                        connected.as_ref().err().unwrap()
                                    );
                                }
                                local_app_sender.queue_message(
                                    MessageTarget::View(view_key),
                                    make_message(WiFiMessages::Connected(connected.is_ok())),
                                );

                                #[cfg(feature = "debug_console")]
                                local_app_sender.queue_message(
                                    MessageTarget::View(view_key),
                                    make_message(ConsoleMessages::AddText(format!(
                                        "Wifi Connect Attempt: {}",
                                        if connected.is_err() {
                                            format!("Failed! ({:?})", connected.err().unwrap())
                                        } else {
                                            "Succeeded!".to_string()
                                        }
                                    ))),
                                );
                            };
                            fasync::Task::local(f).detach();
                        }
                    }
                    _ => {}
                }
                if !field_text.is_empty() {
                    // Fire up a new keyboard
                    let mut keyboard = Box::new(
                        KeyboardViewAssistant::new(self.app_sender.clone(), self.view_key).unwrap(),
                    );
                    keyboard.set_field_name(field_text);
                    keyboard.set_text_field(entry_text);
                    self.app_sender.queue_message(
                        MessageTarget::View(self.view_key),
                        make_message(ProxyMessages::NewViewAssistant(Some(keyboard))),
                    );
                }
            }
        }
    }

    #[cfg(feature = "http_setup_server")]
    fn handle_wifi_message(&mut self, message: &WiFiMessages) {
        self.connected = message.clone();
        self.render_resources = None;
    }
}

impl ViewAssistant for RecoveryViewAssistant {
    fn setup(&mut self, context: &ViewAssistantContext) -> Result<(), Error> {
        self.view_key = context.key;
        Ok(())
    }

    fn render(
        &mut self,
        render_context: &mut RenderContext,
        ready_event: Event,
        context: &ViewAssistantContext,
    ) -> Result<(), Error> {
        // Emulate the size that Carnelian passes when the display is rotated
        let target_size = context.size;

        if self.render_resources.is_none() {
            self.render_resources = Some(RenderResources::new(
                render_context,
                &self.file,
                target_size,
                self.heading,
                self.body.as_ref().map(Borrow::borrow),
                self.countdown_ticks,
                #[cfg(feature = "http_setup_server")]
                &self.wifi_ssid,
                #[cfg(feature = "http_setup_server")]
                &self.wifi_password,
                #[cfg(feature = "http_setup_server")]
                &self.connected,
                &self.face,
                self.reset_state_machine.is_counting_down(),
            ));
        }

        let render_resources = self.render_resources.as_mut().unwrap();
        render_resources.scene.render(render_context, ready_event, context)?;
        context.request_render();
        Ok(())
    }

    fn handle_message(&mut self, message: carnelian::Message) {
        if let Some(message) = message.downcast_ref::<RecoveryMessages>() {
            self.handle_recovery_message(message);
        } else if cfg!(feature = "http_setup_server") {
            #[cfg(feature = "http_setup_server")]
            if let Some(message) = message.downcast_ref::<ButtonMessages>() {
                self.handle_button_message(message);
            } else if let Some(message) = message.downcast_ref::<KeyboardMessages>() {
                self.handle_keyboard_message(message);
            } else if let Some(message) = message.downcast_ref::<WiFiMessages>() {
                self.handle_wifi_message(message);
            } else {
                println!("Unknown message received: {:#?}", message);
            }
        }
    }

    fn handle_consumer_control_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _: &input::Event,
        consumer_control_event: &input::consumer_control::Event,
    ) -> Result<(), Error> {
        match consumer_control_event.button {
            ConsumerControlButton::VolumeUp | ConsumerControlButton::VolumeDown => {
                let state = self.reset_state_machine.handle_event(ResetEvent::ButtonPress(
                    consumer_control_event.button,
                    consumer_control_event.phase,
                ));

                if let fdr::FactoryResetState::AwaitingPolicy(check_id) = state {
                    match self.fdr_restriction {
                        FdrRestriction::Restricted { .. } => {
                            fasync::Task::local(RecoveryViewAssistant::check_fdr_and_maybe_reset(
                                self.view_key,
                                self.app_sender.clone(),
                                check_id,
                            ))
                            .detach();
                        }
                        // When fdr is not restricted, immediately send an enabled event.
                        FdrRestriction::NotRestricted => {
                            let state = self
                                .reset_state_machine
                                .handle_event(ResetEvent::AwaitPolicyResult(check_id, true));
                            if state != fdr::FactoryResetState::ExecuteReset {
                                context.queue_message(make_message(
                                    RecoveryMessages::ResetMessage(state),
                                ));
                            }
                        }
                    }
                } else {
                    context.queue_message(make_message(RecoveryMessages::ResetMessage(state)));
                }
            }
            _ => {}
        }
        Ok(())
    }

    #[cfg(feature = "http_setup_server")]
    fn handle_pointer_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _event: &input::Event,
        pointer_event: &input::pointer::Event,
    ) -> Result<(), Error> {
        if let Some(render_resources) = self.render_resources.as_mut() {
            for button in &mut render_resources.buttons {
                button.handle_pointer_event(&mut render_resources.scene, context, &pointer_event);
            }
        }
        context.request_render();
        Ok(())
    }

    // This is to allow development of this feature on devices without consumer control buttons.
    #[cfg(feature = "http_setup_server")]
    fn handle_keyboard_event(
        &mut self,
        context: &mut ViewAssistantContext,
        event: &input::Event,
        keyboard_event: &input::keyboard::Event,
    ) -> Result<(), Error> {
        const HID_USAGE_KEY_LEFT_BRACKET: u32 = 47;
        const HID_USAGE_KEY_RIGHT_BRACKET: u32 = 48;

        fn keyboard_to_consumer_phase(
            phase: carnelian::input::keyboard::Phase,
        ) -> Option<carnelian::input::consumer_control::Phase> {
            match phase {
                carnelian::input::keyboard::Phase::Pressed => {
                    Some(carnelian::input::consumer_control::Phase::Down)
                }
                carnelian::input::keyboard::Phase::Released => {
                    Some(carnelian::input::consumer_control::Phase::Up)
                }
                _ => None,
            }
        }

        let synthetic_phase = keyboard_to_consumer_phase(keyboard_event.phase);

        let synthetic_event =
            synthetic_phase.and_then(|synthetic_phase| match keyboard_event.hid_usage {
                HID_USAGE_KEY_LEFT_BRACKET => Some(input::consumer_control::Event {
                    button: ConsumerControlButton::VolumeDown,
                    phase: synthetic_phase,
                }),
                HID_USAGE_KEY_RIGHT_BRACKET => Some(input::consumer_control::Event {
                    button: ConsumerControlButton::VolumeUp,
                    phase: synthetic_phase,
                }),
                _ => None,
            });

        if let Some(synthetic_event) = synthetic_event {
            self.handle_consumer_control_event(context, event, &synthetic_event)?;
        }

        Ok(())
    }
}

/// Connects to WiFi, returns a future that will wait for a connection
#[cfg(feature = "http_setup_server")]
async fn connect_to_wifi(ssid: String, password: String) -> Result<(), Error> {
    println!("Connecting to WiFi ");
    use fidl_fuchsia_wlan_policy::SecurityType;
    use recovery_util::wlan::{create_network_info, WifiConnect, WifiConnectImpl};
    let wifi = WifiConnectImpl::new();
    let network = create_network_info(&ssid, Some(&password), Some(SecurityType::Wpa2));
    wifi.connect(network).await
}

/// Determines whether or not fdr is enabled.
async fn check_fdr_enabled() -> Result<bool, Error> {
    let proxy = connect_to_protocol::<FactoryResetPolicyMarker>()?;
    proxy
        .get_enabled()
        .await
        .map_err(|e| format_err!("Could not get status of factory reset: {:?}", e))
}

/// Return the recovery body based on whether or not factory reset is restricted.
const fn get_recovery_body(fdr_enabled: bool) -> Option<&'static str> {
    if fdr_enabled {
        Some(RECOVERY_MODE_BODY)
    } else {
        None
    }
}

fn make_app_assistant_fut(
    app_sender: &AppSender,
) -> LocalBoxFuture<'_, Result<AppAssistantPtr, Error>> {
    let f = async move {
        // Build the fdr restriction depending on whether the fdr restriction config exists,
        // and if so, whether or not the policy api allows fdr.
        let fdr_restriction = {
            let has_restricted_fdr_config = Path::new(PATH_TO_FDR_RESTRICTION_CONFIG).exists();
            if has_restricted_fdr_config {
                let fdr_initially_enabled = check_fdr_enabled().await.unwrap_or_else(|error| {
                    eprintln!("Could not get fdr policy. Falling back to `true`: {:?}", error);
                    true
                });
                FdrRestriction::Restricted { fdr_initially_enabled }
            } else {
                FdrRestriction::NotRestricted
            }
        };

        let assistant = Box::new(RecoveryAppAssistant::new(app_sender, fdr_restriction));
        Ok::<AppAssistantPtr, Error>(assistant)
    };
    Box::pin(f)
}

pub fn make_app_assistant() -> AssistantCreatorFunc {
    Box::new(make_app_assistant_fut)
}

fn main() -> Result<(), Error> {
    println!("recovery: started");
    App::run(make_app_assistant())
}

#[cfg(test)]
mod tests {
    use super::make_app_assistant;
    use carnelian::App;

    #[test]
    fn test_ui() -> std::result::Result<(), anyhow::Error> {
        let assistant = make_app_assistant();
        App::test(assistant)
    }
}
