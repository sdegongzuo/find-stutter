use crate::{collector::Collector, detector::Detector, logger::Logger, Config};
use log::info;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

static RUNNING: AtomicBool = AtomicBool::new(true);

define_windows_service!(ffi_service_main, service_entry);

fn service_entry(_arguments: Vec<OsString>) {
    if let Err(e) = service_loop() {
        log::error!("Service failed: {}", e);
    }
}

pub fn run_service() -> anyhow::Result<()> {
    match service_dispatcher::start("FindStutter", ffi_service_main) {
        Ok(()) => Ok(()),
        Err(_) => {
            RUNNING.store(true, Ordering::SeqCst);
            service_loop()
        }
    }
}

fn service_loop() -> anyhow::Result<()> {
    let handler = service_control_handler::register("FindStutter", move |control_event| {
        match control_event {
            ServiceControl::Stop => {
                RUNNING.store(false, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    handler.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;
    info!("Service started");

    let config = Config::load("config.toml").unwrap_or_default();
    let mut collector = Collector::new();
    let mut detector = Detector::new(&config.detection);
    let mut logger = Logger::new(&config.storage)?;

    let tick = Duration::from_millis(config.sampling.interval_ms);
    while RUNNING.load(Ordering::SeqCst) {
        let sample = collector.collect();
        if let Some(event) = detector.analyze(&sample) {
            log::warn!(
                "Stutter detected: {:?} — {}",
                event.severity,
                event.causes.join(", ")
            );
            let _ = logger.write_event(&event);
        }
        let _ = logger.write_sample(&sample);
        std::thread::sleep(tick);
    }

    logger.flush()?;
    logger.cleanup()?;

    handler.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;
    info!("Service stopped");
    Ok(())
}
