use std;
use std::sync::Arc;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::sync::mpsc;
use rayon;

use super::*;
use graphics;
use resource;

/// `Engine` is the root object of the game application. It binds various sub-systems in
/// a central place and takes take of trivial tasks like the execution order or life-time
/// management.
pub struct Engine {
    min_fps: u32,
    max_fps: u32,
    max_inactive_fps: u32,
    smoothing_step: usize,
    previous_timesteps: VecDeque<Duration>,
    timestep: Duration,
    last_frame_timepoint: Instant,
    alive: bool,
    scheduler: rayon::ThreadPool,

    pub input: input::Input,
    pub window: Arc<graphics::Window>,
    pub graphics: graphics::GraphicsSystem,
    pub resources: resource::ResourceSystem,
}

impl Engine {
    /// Constructs a new, empty engine.
    pub fn new() -> Result<Self> {
        Engine::new_with(Settings::default())
    }

    /// Setup engine with specified settings.
    pub fn new_with(settings: Settings) -> Result<Self> {
        let mut wb = graphics::WindowBuilder::new();
        wb.with_title(settings.window.title.clone())
            .with_dimensions(settings.window.width, settings.window.height);

        let input = input::Input::new();
        let window = Arc::new(wb.build(&input)?);
        let graphics = graphics::GraphicsSystem::new(window.clone())?;

        let confs = rayon::Configuration::new();
        let scheduler = rayon::ThreadPool::new(confs).unwrap();

        Ok(Engine {
               min_fps: settings.engine.min_fps,
               max_fps: settings.engine.max_fps,
               max_inactive_fps: settings.engine.max_inactive_fps,
               smoothing_step: settings.engine.time_smooth_step as usize,
               previous_timesteps: VecDeque::new(),
               timestep: Duration::new(0, 0),
               last_frame_timepoint: Instant::now(),
               alive: true,
               scheduler: scheduler,

               input: input,
               window: window,
               graphics: graphics,
               resources: resource::ResourceSystem::new()?,
           })
    }

    pub fn shared(&self) -> FrameShared {
        FrameShared {
            video: self.graphics.shared(),
            resource: self.resources.shared(),
        }
    }

    /// Run the main loop of `Engine`, this will block the working
    /// thread until we finished.
    pub fn run<T>(mut self, application: T) -> Result<Self>
        where T: Application + Send + Sync + 'static
    {
        let application = Arc::new(RwLock::new(application));

        let dir = ::std::env::current_dir()?;
        println!("Run crayon-runtim with working directory {:?}.", dir);

        let mut events = Vec::new();
        'main: while self.alive {
            // Poll any possible events first.
            events.clear();

            self.input.run_one_frame(&mut events);
            for v in events.drain(..) {
                match v {
                    event::Event::Application(value) => {
                        match value {
                            event::ApplicationEvent::Closed => {
                                self.stop();
                                break 'main;
                            }
                            other => println!("Drop {:?}.", other),
                        };
                    }
                    event::Event::InputDevice(value) => self.input.process(value),
                    other => println!("Drop {:?}.", other),
                }
            }

            self.advance();
            self.graphics.swap_frames();

            // Perform update and render submitting for frame [x], and drawing frame [x-1]
            // at the same time.
            let video_info = {
                let shared = self.shared();
                let application = application.clone();
                let (rx, tx) = mpsc::channel();

                let closure = move || {
                    let v = Engine::execute_frame(application, shared);
                    rx.send(v).unwrap();
                };

                self.scheduler.spawn(closure);
                // This will block the main-thread until all the graphics commands
                // is finished by GPU.
                let video_info = self.graphics.advance().unwrap();
                tx.recv().unwrap()?;
                video_info
            };

            // Advance resource system.
            let resource_info = self.resources.advance()?;

            //
            let info = FrameInfo {
                video: video_info,
                resource: resource_info,
            };

            //
            {
                let mut shared = self.shared();
                let application = application.clone();
                self.scheduler
                    .install(|| {
                                 let mut application = application.write().unwrap();
                                 application.on_post_update(&mut shared, &info)
                             })?;
            }
        }

        Ok(self)
    }

    fn execute_frame(application: Arc<RwLock<Application>>, mut shared: FrameShared) -> Result<()> {
        let mut application = application.write().unwrap();
        application.on_update(&mut shared)?;
        application.on_render(&mut shared)?;

        Ok(())
    }

    /// Stop the whole application.
    pub fn stop(&mut self) {
        self.alive = false;
    }

    /// Advance one frame.
    pub fn advance(&mut self) -> Duration {
        // Perform waiting loop if maximum fps set, cooperatively gives up
        // a timeslice to the OS scheduler.
        if self.max_fps > 0 {
            let td = Duration::from_millis((1000 / self.max_fps) as u64);
            while self.last_frame_timepoint.elapsed() <= td {
                if (self.last_frame_timepoint.elapsed() + Duration::from_millis(5)) < td {
                    std::thread::sleep(Duration::from_millis(1));
                } else {
                    std::thread::yield_now();
                }
            }
        }

        let mut elapsed = self.last_frame_timepoint.elapsed();
        self.last_frame_timepoint = Instant::now();

        // If fps lower than minimum, simply clamp it.
        if self.min_fps > 0 {
            elapsed = std::cmp::min(elapsed, Duration::from_millis((1000 / self.min_fps) as u64));
        }

        // Perform timestep smoothing.
        if self.smoothing_step > 0 {
            self.previous_timesteps.push_front(elapsed);
            if self.previous_timesteps.len() > self.smoothing_step {
                self.previous_timesteps.drain(self.smoothing_step..);

                self.timestep = Duration::new(0, 0);
                for step in self.previous_timesteps.iter() {
                    self.timestep += *step;
                }
                self.timestep /= self.previous_timesteps.len() as u32;
            } else {
                self.timestep = *self.previous_timesteps.front().unwrap();
            }
        } else {
            self.timestep = elapsed;
        }

        self.timestep
    }

    /// Set minimum frames per second. If fps goes lower than this, time will
    /// appear to slow. This is useful for some subsystems required strict minimum
    /// time step per frame, such like Collision checks.
    #[inline]
    pub fn set_min_fps(&mut self, fps: u32) {
        self.min_fps = fps;
    }

    /// Set maximum frames per second. The engine will sleep if fps is higher
    /// than this for less resource(e.g. power) consumptions.
    #[inline]
    pub fn set_max_fps(&mut self, fps: u32) {
        self.max_fps = fps;
    }

    /// Set maximum frames per second when the application does not have input
    /// focus.
    #[inline]
    pub fn set_max_inactive_fps(&mut self, fps: u32) {
        self.max_inactive_fps = fps;
    }

    /// Set how many frames to average for timestep smoothing.
    #[inline]
    pub fn set_time_smoothing_step(&mut self, step: u32) {
        self.smoothing_step = step as usize;
    }

    /// Get current fps.
    #[inline]
    pub fn get_fps(&self) -> u32 {
        if self.timestep.subsec_nanos() == 0 {
            0
        } else {
            (1000000000.0 / self.timestep.subsec_nanos() as f64) as u32
        }
    }

    /// Returns timestep of last frame.
    #[inline]
    pub fn timestep_in_seconds(&self) -> f32 {
        let sec = self.timestep.as_secs();
        let nansec = self.timestep.subsec_nanos() as u64;
        sec as f32 + (nansec as f32 * 1e-9)
    }
}