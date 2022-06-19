use itertools::Itertools;

use image::{
    self, codecs::gif::GifDecoder, imageops::FilterType, AnimationDecoder, DynamicImage,
    ImageFormat,
};
use log::debug;

use smithay_client_toolkit::reexports::calloop::channel::SyncSender;

use std::{
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::Answer;
pub mod comp_decomp;
use comp_decomp::{BitPack, ReadiedPack};

///Note: since this entire struct will be going to a new thread, it has to own all of its values.
///This means even though, in the case of multiple outputs with different dimensions, they would
///all have the same path, filter, step and fps, we still need to store all those values multiple
///times, because we would simply have to clone them when moving them into the thread anyway
pub struct ProcessorRequest {
    pub outputs: Vec<String>,
    pub dimensions: (u32, u32),
    pub old_img: Box<[u8]>,
    pub path: PathBuf,
    pub filter: FilterType,
    pub step: u8,
    pub fps: Duration,
}

impl ProcessorRequest {
    fn split(self) -> (Vec<String>, Transition, Option<GifProcessor>) {
        let transition = Transition {
            old_img: self.old_img,
            step: self.step,
            fps: self.fps,
        };
        let img = image::io::Reader::open(&self.path);
        let animation = {
            if let Ok(img) = img {
                if img.format() == Some(ImageFormat::Gif) {
                    Some(GifProcessor {
                        gif: self.path,
                        dimensions: self.dimensions,
                        filter: self.filter,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };
        (self.outputs, transition, animation)
    }
}

struct Transition {
    old_img: Box<[u8]>,
    step: u8,
    fps: Duration,
}

/// All transitions return whether or not they completed
impl Transition {
    fn default(
        mut self,
        new_img: &[u8],
        outputs: &mut Vec<String>,
        sender: &SyncSender<(Vec<String>, ReadiedPack)>,
        stop_recv: &mpsc::Receiver<Vec<String>>,
    ) -> bool {
        let mut now = Instant::now();
        let mut transition: Vec<u8> = vec![255; new_img.len()];
        let mut done;
        loop {
            done = true;
            let trans_chunks = bytemuck::cast_slice_mut::<u8, [u8; 4]>(&mut transition);
            let old_chunks = bytemuck::cast_slice::<u8, [u8; 4]>(&self.old_img);
            let new_chunks = bytemuck::cast_slice::<u8, [u8; 4]>(new_img);

            let outer_for = trans_chunks
                .iter_mut()
                .zip_eq(old_chunks.iter().zip_eq(new_chunks));
            for (trans_pix, (old_pix, new_pix)) in outer_for {
                let inner_for = trans_pix
                    .iter_mut()
                    .zip_eq(old_pix.iter().zip_eq(new_pix.iter()))
                    .take(3);
                for (trans_col, (old_col, new_col)) in inner_for {
                    let distance = if old_col > new_col {
                        old_col - new_col
                    } else {
                        new_col - old_col
                    };
                    if distance < self.step {
                        *trans_col = *new_col;
                    } else if old_col > new_col {
                        done = false;
                        *trans_col = *old_col - self.step;
                    } else {
                        done = false;
                        *trans_col = *old_col + self.step;
                    }
                }
            }

            let compressed_img = ReadiedPack::new(&self.old_img, &transition);
            let timeout = self.fps.saturating_sub(now.elapsed());
            if send_frame(compressed_img, outputs, timeout, sender, stop_recv) {
                debug!("Transition was interrupted!");
                return false;
            };
            now = Instant::now();
            if done {
                debug!("Transition has finished.");
                return true;
            }
            self.old_img.clone_from_slice(&transition);
        }
    }
}

struct GifProcessor {
    gif: PathBuf,
    dimensions: (u32, u32),
    filter: FilterType,
}

impl GifProcessor {
    fn process(self, first_frame: Box<[u8]>, fr_sender: mpsc::Sender<(BitPack, Duration)>) {
        let gif_reader = image::io::Reader::open(self.gif).unwrap();
        let mut frames = GifDecoder::new(gif_reader.into_inner())
            .expect("Couldn't decode gif, though this should be impossible...")
            .into_frames();
        //The first frame should always exist
        let dur_first_frame = frames.next().unwrap().unwrap().delay().numer_denom_ms();
        let dur_first_frame = Duration::from_millis((dur_first_frame.0 / dur_first_frame.1).into());

        let mut canvas = first_frame.clone();
        while let Some(Ok(frame)) = frames.next() {
            let (dur_num, dur_div) = frame.delay().numer_denom_ms();
            let duration = Duration::from_millis((dur_num / dur_div).into());
            let img = img_resize(frame.into_buffer(), self.dimensions, self.filter);

            if fr_sender
                .send((BitPack::pack(&canvas, &img), duration))
                .is_err()
            {
                return;
            };
            canvas = img;
        }
        //Add the first frame we got earlier:
        let _ = fr_sender.send((BitPack::pack(&canvas, &first_frame), dur_first_frame));
    }
}

pub struct Processor {
    frame_sender: SyncSender<(Vec<String>, ReadiedPack)>,
    anim_stoppers: Vec<mpsc::SyncSender<Vec<String>>>,
}

impl Processor {
    pub fn new(frame_sender: SyncSender<(Vec<String>, ReadiedPack)>) -> Self {
        Self {
            anim_stoppers: Vec::new(),
            frame_sender,
        }
    }

    pub fn process(&mut self, requests: Vec<ProcessorRequest>) -> Answer {
        for request in requests {
            let img = match image::open(&request.path) {
                Ok(i) => i.into_rgba8(),
                Err(e) => {
                    return Answer::Err(format!(
                        "failed to open image '{:#?}': {}",
                        &request.path, e
                    ))
                }
            };
            self.stop_animations(&request.outputs);

            let new_img = img_resize(img, request.dimensions, request.filter);
            self.transition(request, new_img);
        }
        debug!("Finished image processing!");
        Answer::Ok
    }

    pub fn stop_animations(&mut self, to_stop: &[String]) {
        self.anim_stoppers
            .retain(|a| a.send(to_stop.to_vec()).is_ok());
    }

    fn transition(&mut self, request: ProcessorRequest, new_img: Box<[u8]>) {
        let sender = self.frame_sender.clone();
        let (stopper, stop_recv) = mpsc::sync_channel(1);
        self.anim_stoppers.push(stopper);
        thread::spawn(move || {
            let (mut out, transition, gif) = request.split();
            if transition.default(&new_img, &mut out, &sender, &stop_recv) {
                if let Some(gif) = gif {
                    animation(gif, new_img, out, sender, stop_recv);
                }
            }
        });
    }
}

impl Drop for Processor {
    //We need to make sure pending animators exited
    fn drop(&mut self) {
        while !self.anim_stoppers.is_empty() {
            self.stop_animations(&Vec::new());
        }
    }
}

fn animation(
    gif: GifProcessor,
    new_img: Box<[u8]>,
    mut outputs: Vec<String>,
    sender: SyncSender<(Vec<String>, ReadiedPack)>,
    stopper: mpsc::Receiver<Vec<String>>,
) {
    let img_len = new_img.len();
    let mut cached_frames = Vec::new();
    let mut now = Instant::now();
    {
        let (fr_send, fr_recv) = mpsc::channel();
        let handle = thread::spawn(move || gif.process(new_img, fr_send));
        while let Ok((fr, dur)) = fr_recv.recv() {
            let frame = fr.ready(img_len);
            let timeout = dur.saturating_sub(now.elapsed());
            if send_frame(frame, &mut outputs, timeout, &sender, &stopper) {
                let _ = handle.join();
                return;
            };
            now = Instant::now();
            cached_frames.push((fr, dur));
        }
        let _ = handle.join();
    }
    let cached_frames = cached_frames.into_boxed_slice();
    if cached_frames.len() > 1 {
        loop {
            for (fr, dur) in cached_frames.iter() {
                let frame = fr.ready(img_len);
                let timeout = dur.saturating_sub(now.elapsed());
                if send_frame(frame, &mut outputs, timeout, &sender, &stopper) {
                    return;
                };
                now = Instant::now();
            }
        }
    }
}

fn img_resize(img: image::RgbaImage, dimensions: (u32, u32), filter: FilterType) -> Box<[u8]> {
    let (width, height) = dimensions;
    debug!("Output dimensions: {:?}", (width, height));
    debug!("Image dimensions:  {:?}", img.dimensions());
    let mut resized_img = if img.dimensions() != (width, height) {
        debug!("Image dimensions are different from output's. Resizing...");
        DynamicImage::ImageRgba8(img)
            .resize_to_fill(width, height, filter)
            .into_rgba8()
            .into_raw()
    } else {
        img.into_raw()
    };

    // The ARGB is 'little endian', so here we must  put the order
    // of bytes 'in reverse', so it needs to be BGRA.
    for pixel in resized_img.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    resized_img.into_boxed_slice()
}

///Returns whether the calling function should exit or not
fn send_frame(
    frame: ReadiedPack,
    outputs: &mut Vec<String>,
    timeout: Duration,
    sender: &SyncSender<(Vec<String>, ReadiedPack)>,
    stop_recv: &mpsc::Receiver<Vec<String>>,
) -> bool {
    match stop_recv.recv_timeout(timeout) {
        Ok(to_remove) => {
            outputs.retain(|o| !to_remove.contains(o));
            if outputs.is_empty() || to_remove.is_empty() {
                return true;
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => (),
        Err(mpsc::RecvTimeoutError::Disconnected) => return true,
    }
    match sender.send((outputs.clone(), frame)) {
        Ok(()) => false,
        Err(_) => true,
    }
}
