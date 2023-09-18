// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]
#![forbid(unsafe_code)]

//! Gnome search provider for Jetbrains products

use anyhow::{Context, Result};
use std::mem::forget;
use std::sync::{Arc, Mutex};
use tracing::{event, Level};
use tracing_futures::Instrument;

use gnome_search_provider_common::app::*;
use gnome_search_provider_common::futures_channel::mpsc;
use gnome_search_provider_common::gio;
use gnome_search_provider_common::gio::prelude::{SettingsExt, SettingsExtManual};
use gnome_search_provider_common::gio::{glib, Settings};
use gnome_search_provider_common::logging::*;
use gnome_search_provider_common::mainloop::*;
use gnome_search_provider_common::serviceinterface::ServiceInterface;
use gnome_search_provider_common::zbus;

mod config;
mod providers;
mod searchprovider;

use providers::*;
use searchprovider::*;

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.Jetbrains";

async fn tick(connection: zbus::Connection) {
    loop {
        connection.executor().tick().await
    }
}

/// The running service.
#[derive(Debug)]
struct Service {
    /// The launch service used to launch applications.
    launch_service: AppLaunchService,
    /// The DBus connection of this service.
    connection: zbus::Connection,
}

/// Starts the DBUS service.
///
/// Connect to the session bus and register a new DBus object for every provider
/// whose underlying app is installed.
///
/// Then register the connection on the Glib main loop and install a callback to
/// handle incoming messages.
async fn start_dbus_service(log_control: LogControl) -> Result<Service> {
    let launch_service = AppLaunchService::new();

    let mut providers = Vec::with_capacity(PROVIDERS.len());
    let mut search_services = Vec::with_capacity(PROVIDERS.len());
    let settings = Settings::new("de.swsnr.searchprovider.jetbrains");
    let disabled_apps: Vec<String> = SettingsExtManual::get(&settings, "disabled");
    event!(Level::INFO, "Disabled apps are: {:?}", disabled_apps);
    let mut app_disabled_state: Vec<(String, Arc<Mutex<bool>>)> =
        Vec::with_capacity(PROVIDERS.len());
    for provider in PROVIDERS {
        if let Some(gio_app) = gio::DesktopAppInfo::new(provider.desktop_id) {
            event!(Level::INFO, "Found app {}", provider.desktop_id);
            let app_id = AppId::from(&gio_app);

            let (tx, rx) = mpsc::channel(8);
            search_services.push((app_id.clone(), tx.clone()));

            event!(Level::DEBUG, %app_id, "Starting search service for {}", &app_id);
            // Move IO to a separate thread pool to avoid blocking the main loop.
            // We use a shared pool to share two threads among all providers.
            let io_pool = glib::ThreadPool::shared(Some(2)).with_context(|| {
                format!(
                    "Failed to create thread pool to read recent projects for app {}",
                    &app_id
                )
            })?;
            glib::MainContext::ref_thread_default().spawn(serve_search_provider(
                app_id.clone(),
                &provider.config,
                io_pool,
                rx,
            ));

            let mut search_provider_extensions =
                SearchProviderExtensions::new((&gio_app).into(), tx.clone());
            let app: App = gio_app.into();
            let mut lock = app.disabled.lock();
            match lock {
                Ok(ref mut guard) => {
                    let disabled = disabled_apps.contains(&app_id.to_string());
                    event!(
                        Level::INFO,
                        "App {} is now {}",
                        app_id,
                        if disabled { "disabled" } else { "enabled" }
                    );
                    **guard = disabled
                }
                Err(_) => event!(
                    Level::WARN,
                    "Cannot acquire disable lock for app {}, app will be searched for projects",
                    app_id
                ),
            }
            drop(lock);
            app_disabled_state.push((app_id.to_string(), app.disabled.clone()));
            let search_provider = AppItemSearchProvider::new(app, launch_service.client(), tx);
            let _ = search_provider_extensions.refresh().in_current_span().await;

            providers.push((
                provider.objpath(),
                search_provider,
                search_provider_extensions,
            ));
        } else {
            event!(
                Level::DEBUG,
                desktop_id = provider.desktop_id,
                "Skipping provider, app not found"
            );
        }
    }

    event!(
        Level::DEBUG,
        "Connecting to session bus, registering interfaces for {} providers, and acquiring {}",
        providers.len(),
        BUSNAME
    );
    let service_interface = ServiceInterface::new(search_services);
    // We disable the internal executor because we'd like to run the connection
    // exclusively on the glib mainloop, and thus tick it manually (see below).
    let connection = providers
        .into_iter()
        .try_fold(
            zbus::ConnectionBuilder::session()?.internal_executor(false),
            |builder, (path, provider, extensions)| {
                event!(
                    Level::DEBUG,
                    app_id = %provider.app().id(),
                    "Serving search provider for {} at {}",
                    provider.app().id(),
                    &path
                );
                builder
                    .serve_at(path.clone(), provider)?
                    .serve_at(path, extensions)
            },
        )?
        .serve_at("/", service_interface)?
        .serve_at("/org/freedesktop/LogControl1", log_control)?
        .name(BUSNAME)?
        .build()
        .await
        .with_context(|| "Failed to connect to session bus")?;

    event!(
        Level::DEBUG,
        "Register signal callback when disabled settings change"
    );
    settings.connect_changed(None, move |s, k| {
        let disabled_apps: Vec<String> = s.get(k);
        event!(
            Level::INFO,
            "Disabled settings {} changed to {:?}",
            k,
            disabled_apps
        );
        app_disabled_state.iter().for_each(|(app_id, disabled)| {
            let mut lock = disabled.lock();
            match lock {
                Ok(ref mut guard) => **guard = disabled_apps.contains(app_id),
                Err(_) => event!(
                    Level::WARN,
                    "Lock for app {} cannot be acquired, disable state remains",
                    app_id
                ),
            }
        });
    });
    // When the 'settings' variable would be dropped, the changed listener would be removed as well
    forget(settings);
    // Manually tick the connection on the glib mainloop to make all code in zbus run on the mainloop.
    glib::MainContext::ref_thread_default().spawn(tick(connection.clone()));

    event!(
        Level::INFO,
        "Acquired name {}, serving search providers",
        BUSNAME
    );
    Ok(Service {
        launch_service,
        connection,
    })
}

fn app() -> clap::Command {
    use clap::*;
    command!()
        .dont_collapse_args_in_usage(true)
        .term_width(80)
        .after_help(
            "\
Set $RUST_LOG to control the log level",
        )
        .arg(
            Arg::new("providers")
                .long("providers")
                .action(ArgAction::SetTrue)
                .help("List all providers"),
        )
}

fn main() {
    let matches = app().get_matches();
    if matches.get_flag("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{label}")
        }
    } else {
        let log_control = setup_logging_for_service();

        event!(
            Level::INFO,
            "Started {} version: {}",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION")
        );

        match glib::MainContext::ref_thread_default().block_on(start_dbus_service(log_control)) {
            Ok(service) => {
                let _ = service.launch_service.start(
                    service.connection,
                    SystemdScopeSettings {
                        prefix: concat!("app-", env!("CARGO_BIN_NAME")).to_string(),
                        started_by: env!("CARGO_BIN_NAME").to_string(),
                        documentation: vec![env!("CARGO_PKG_HOMEPAGE").to_string()],
                    },
                );
                create_main_loop(&glib::MainContext::ref_thread_default()).run();
            }
            Err(error) => {
                event!(Level::ERROR, %error, "Failed to start DBus server: {:#}", error);
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_app() {
        app().debug_assert();
    }
}
