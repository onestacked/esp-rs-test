#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::{cell::RefCell, mem::size_of_val};

use critical_section::Mutex;
use esp_hal::{
    clock::ClockControl,
    cpu_control::{self, CpuControl},
    delay::Delay,
    embassy, entry,
    gpio::{AnyPin, Output, PushPull, IO},
    macros::main,
    peripherals::Peripherals,
    rng::Rng,
    system::SystemExt,
    timer::TimerGroup,
    xtensa_lx::singleton,
};

use esp_backtrace as _;
use esp_println as _;

use embassy_executor::Spawner;
use embassy_time::Duration;

use esp_wifi::{
    initialize,
    wifi::{
        AuthMethod, ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent,
        WifiStaDevice, WifiState,
    },
    EspWifiInitFor,
};

use embassy_net::{Stack, StackResources};
use log::info;
use picoserve::{
    response::File,
    routing::{get, parse_path_segment},
};

type AppRouter = impl picoserve::routing::PathRouter;

const WEB_TASK_POOL_SIZE: usize = 2;

extern crate alloc;
#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

fn init_heap() {
    use core::mem::MaybeUninit;
    const HEAP_SIZE: usize = 32 * 1024;

    #[link_section = ".dram2_uninit"]
    static mut HEAP: MaybeUninit<[u8; HEAP_SIZE]> = MaybeUninit::uninit();

    unsafe {
        ALLOCATOR.init(HEAP.as_mut_ptr() as *mut u8, HEAP_SIZE);
    }
}

extern "C" {
    static _bss_start: u32;
    static _bss_end: u32;

    static _data_start: u32;
    static _data_end: u32;

    static _stack_start: u32;
    static _stack_end: u32;

    static _sidata: u32;
}
fn log_memory_stats<const C: usize>(app_stack: &mut cpu_control::Stack<C>) {
    unsafe {
        let bss_start = (&_bss_start) as *const _ as usize;
        let bss_end = (&_bss_end) as *const _ as usize;
        info!(
            "bss:       {:08X}-{:08X} {:08X}",
            bss_start,
            bss_end,
            bss_end - bss_start
        );

        let data_start = (&_data_start) as *const _ as usize;
        let data_end = (&_data_end) as *const _ as usize;
        info!(
            "data:      {:08X}-{:08X} {:08X}",
            data_start,
            data_end,
            data_end - data_start
        );
        let stack_start = (&_stack_start) as *const _ as usize;
        let stack_end = (&_stack_end) as *const _ as usize;
        info!(
            "stack:     {:08X}-{:08X} {:08X}",
            stack_end,
            stack_start,
            stack_start - stack_end
        );

        let app_stack_start = (&app_stack) as *const _ as usize;
        let app_stack_end = app_stack_start + size_of_val(app_stack);
        info!(
            "app_stack: {:08X}-{:08X} {:08X}",
            app_stack_start,
            app_stack_end,
            app_stack_end - app_stack_start
        );
    }
}

#[toml_cfg::toml_config]
pub struct Config {
    #[default("Wifi-SSID")]
    ssid: &'static str,

    #[default(None)]
    password: &'static str,
}

fn make_app() -> picoserve::Router<AppRouter> {
    picoserve::Router::new()
        .route("/", get(|| File::html(include_str!("index.html"))))
        .route(
            ("/delay", parse_path_segment()),
            get(|secs| async move {
                info!("Starting delay");
                embassy_time::Timer::after(Duration::from_secs(secs)).await;
                info!("Delay done");
                "Done!\n"
            }),
        )
        .route("test", get(|| File::html("Test1")))
        .route("test", get(|| File::html("Test2")))
}

fn cpu1_task(delay: Delay, counter: &critical_section::Mutex<RefCell<i32>>) -> ! {
    info!("Hello World - Core 1!");
    loop {
        delay.delay_millis(100);

        critical_section::with(|cs| {
            let new_val = counter.borrow_ref_mut(cs).wrapping_add(1);
            *counter.borrow_ref_mut(cs) = new_val;
        });
    }
}

#[main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    info!("Booting");
    let app_stack = singleton!(:cpu_control::Stack<8192> = cpu_control::Stack::new()).unwrap();
    log_memory_stats(app_stack);
    init_heap();

    let peripherals = Peripherals::take();
    esp_hal::psram::init_psram(peripherals.PSRAM);

    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();

    let timer = TimerGroup::new(peripherals.TIMG1, &clocks, None).timer0;
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    // set wifi mode
    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*singleton!(
        :Stack<WifiDevice<'static, WifiStaDevice>> = Stack::new(
        wifi_interface,
        config,
        singleton!(:StackResources<8> = StackResources::new()).unwrap(),
        seed
    ))
    .unwrap();

    info!("hello world!");

    // Set GPIO0 as an output, and set its state high initially.
    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut led = io.pins.gpio33.into_push_pull_output();

    led.set_high();

    let timg0 = TimerGroup::new_async(peripherals.TIMG0, &clocks);
    info!("embassy init!");
    embassy::init(&clocks, timg0);

    spawner.spawn(toggle(led.into())).ok();

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    let app = singleton!(:picoserve::Router<AppRouter> = make_app()).unwrap();

    let config =
        singleton!(:picoserve::Config<Duration> = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: Some(Duration::from_secs(5)),
        read_request: Some(Duration::from_secs(1)),
        write: Some(Duration::from_secs(1)),
    })
    .keep_connection_alive())
        .unwrap();

    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, app, config));
    }

    info!("Starting second code");

    let counter = Mutex::new(RefCell::new(0));

    let mut cpu_control = CpuControl::new(system.cpu_control);
    let _guard = cpu_control.start_app_core(app_stack, || {
        cpu1_task(Delay::new(&clocks), &counter);
    });

    loop {
        //info!("main loop!");
        let count = critical_section::with(|cs| *counter.borrow_ref(cs));
        info!("Hello World - Core 0! Counter is {}", count);
        embassy_time::Timer::after(Duration::from_millis(5_000)).await;
    }
}

#[embassy_executor::task]
async fn toggle(mut led: AnyPin<Output<PushPull>>) {
    loop {
        //info!("toggle loop!");
        led.toggle();
        embassy_time::Timer::after_secs(1).await;
        led.toggle();
        embassy_time::Timer::after_secs(1).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                embassy_time::Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: CONFIG.ssid.try_into().expect("SSID to long"),
                password: CONFIG.password.try_into().expect("password to long"),
                auth_method: if CONFIG.password == "" {
                    AuthMethod::WPA2Personal
                } else {
                    AuthMethod::None
                },
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                info!("Failed to connect to wifi: {e:?}");
                embassy_time::Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    app: &'static picoserve::Router<AppRouter>,
    config: &'static picoserve::Config<Duration>,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::listen_and_serve(
        id,
        app,
        config,
        stack,
        port,
        &mut tcp_rx_buffer,
        &mut tcp_tx_buffer,
        &mut http_buffer,
    )
    .await
}
