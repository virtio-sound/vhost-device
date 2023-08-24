// Pipewire backend device
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

use super::AudioBackend;
use crate::vhu_sound::NR_STREAMS;
use crate::virtio_sound::{
    VIRTIO_SND_D_INPUT, VIRTIO_SND_D_OUTPUT, VIRTIO_SND_PCM_FMT_S16, VIRTIO_SND_PCM_FMT_S32,
    VIRTIO_SND_PCM_FMT_S8, VIRTIO_SND_PCM_FMT_U16, VIRTIO_SND_PCM_FMT_U32, VIRTIO_SND_PCM_FMT_U8,
    VIRTIO_SND_PCM_RATE_32000, VIRTIO_SND_PCM_RATE_44100, VIRTIO_SND_PCM_RATE_48000,
    VIRTIO_SND_PCM_RATE_64000, VIRTIO_SND_R_PCM_PREPARE, VIRTIO_SND_R_PCM_RELEASE,
    VIRTIO_SND_R_PCM_SET_PARAMS, VIRTIO_SND_R_PCM_START, VIRTIO_SND_R_PCM_STOP,
};
use crate::Error;
use crate::PCMParams;
use crate::Result;
use std::{
    cmp,
    collections::HashMap,
    convert::TryInto,
    ops::Deref,
    os::raw::c_void,
    ptr,
    ptr::NonNull,
    sync::{Arc, RwLock},
};

use log::debug;
use pipewire as pw;
use pw::spa::param::ParamType;
use std::mem::size_of;

use pw::spa::sys::{
    spa_audio_info_raw, spa_callbacks, spa_format_audio_raw_build, spa_pod, spa_pod_builder,
    spa_pod_builder_state, spa_ringbuffer, spa_ringbuffer_get_read_index, spa_ringbuffer_read_data,
    spa_ringbuffer_read_update, SPA_PARAM_EnumFormat, SPA_AUDIO_CHANNEL_MONO, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR,
    SPA_AUDIO_FORMAT_S16, SPA_AUDIO_FORMAT_S32, SPA_AUDIO_FORMAT_S8, SPA_AUDIO_FORMAT_U16,
    SPA_AUDIO_FORMAT_U32, SPA_AUDIO_FORMAT_U8, SPA_AUDIO_FORMAT_UNKNOWN, SPA_AUDIO_CHANNEL_UNKNOWN
};
use pw::spa::sys::{spa_ringbuffer_write_update, spa_ringbuffer_write_data, spa_ringbuffer_get_write_index};


use pw::stream::ListenerBuilderT;
use pw::sys::{
    pw_buffer, pw_loop, pw_thread_loop, pw_thread_loop_get_loop, pw_thread_loop_lock,
    pw_thread_loop_new, pw_thread_loop_signal, pw_thread_loop_start, pw_thread_loop_unlock,
    pw_thread_loop_wait, PW_ID_CORE,
};
use pw::{properties, spa, Context, Core, LoopRef};

const RINGBUFFER_SIZE: u32 = 1 << 22;
const RINGBUFFER_MASK: u32 = RINGBUFFER_SIZE - 1;
struct PwThreadLoop(NonNull<pw_thread_loop>);

impl PwThreadLoop {
    pub fn new(name: Option<&str>) -> Option<Self> {
        let inner = unsafe {
            pw_thread_loop_new(
                name.map_or(ptr::null(), |p| p.as_ptr() as *const _),
                std::ptr::null_mut(),
            )
        };
        if inner.is_null() {
            None
        } else {
            Some(Self(
                NonNull::new(inner).expect("pw_thread_loop can't be null"),
            ))
        }
    }

    pub fn get_loop(&self) -> PwThreadLoopTheLoop {
        let inner = unsafe { pw_thread_loop_get_loop(self.0.as_ptr()) };
        PwThreadLoopTheLoop(NonNull::new(inner).unwrap())
    }

    pub fn unlock(&self) {
        unsafe { pw_thread_loop_unlock(self.0.as_ptr()) }
    }

    pub fn lock(&self) {
        unsafe { pw_thread_loop_lock(self.0.as_ptr()) }
    }

    pub fn start(&self) {
        unsafe {
            pw_thread_loop_start(self.0.as_ptr());
        }
    }

    pub fn signal(&self) {
        unsafe {
            pw_thread_loop_signal(self.0.as_ptr(), false);
        }
    }

    pub fn wait(&self) {
        unsafe {
            pw_thread_loop_wait(self.0.as_ptr());
        }
    }
}

#[derive(Debug, Clone)]
struct PwThreadLoopTheLoop(NonNull<pw_loop>);

impl AsRef<LoopRef> for PwThreadLoopTheLoop {
    fn as_ref(&self) -> &LoopRef {
        self.deref()
    }
}

impl Deref for PwThreadLoopTheLoop {
    type Target = LoopRef;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.0.as_ptr() as *mut LoopRef) }
    }
}

// SAFETY: Safe as the structure can be sent to another thread.
unsafe impl Send for PwBackend {}

// SAFETY: Safe as the structure can be shared with another thread as the state
// is protected with a lock.
unsafe impl Sync for PwBackend {}

pub struct PwBackend {
    thread_loop: Arc<PwThreadLoop>,
    pub core: Core,
    #[allow(dead_code)]
    context: Context<PwThreadLoopTheLoop>,
    pub stream_params: RwLock<Vec<PCMParams>>,
    pub stream_states: RwLock<Vec<u32>>,
    pub stream_hash: RwLock<HashMap<u32, pw::stream::Stream<i32>>>,
    pub stream_listener: RwLock<HashMap<u32, pw::stream::StreamListener<i32>>>,
    // this is per stream
    // Arc here may be wrong
    pub stream_spa_ringbuffer: Arc<RwLock<Vec<spa_ringbuffer>>>,
    pub stream_buffers: Arc<RwLock<Vec<Vec<u8>>>>,
    pub highwater_mark: Arc<RwLock<u32>>,
}

impl PwBackend {
    pub fn new() -> Self {
        pw::init();

        let thread_loop = Arc::new(PwThreadLoop::new(Some("Pipewire thread loop")).unwrap());
        let get_loop = thread_loop.get_loop();

        thread_loop.lock();

        let context = pw::Context::new(&get_loop).expect("failed to create context");
        thread_loop.start();
        let core = context.connect(None).expect("Failed to connect to core");

        // Create new reference for the variable so that it can be moved into the closure.
        let thread_clone = thread_loop.clone();

        // Trigger the sync event. The server's answer won't be processed until we start the thread loop,
        // so we can safely do this before setting up a callback. This lets us avoid using a Cell.
        let pending = core.sync(0).expect("sync failed");
        let _listener_core = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == PW_ID_CORE && seq == pending {
                    thread_clone.signal();
                }
            })
            .register();

        thread_loop.wait();
        thread_loop.unlock();

        println!("pipewire backend running");

        let streams_param = vec![PCMParams::default(); NR_STREAMS];
        let streams_states = vec![0; NR_STREAMS];
        let stream_spa_ringbuffer = vec![spa_ringbuffer {
            readindex : 0 ,
            writeindex : 0
             }; NR_STREAMS];
        // If the device has an intermediate buffer,
        // its size MUST be no less than
        // the specified buffer_bytes value.
        let buff = vec![0 as u8; RINGBUFFER_SIZE as usize];
        let streams_buffers = vec![buff; NR_STREAMS];

        Self {
            thread_loop,
            core,
            context,
            stream_params: RwLock::new(streams_param),
            stream_states: RwLock::new(streams_states),
            stream_hash: RwLock::new(HashMap::with_capacity(NR_STREAMS)),
            stream_listener: RwLock::new(HashMap::with_capacity(NR_STREAMS)),
            stream_spa_ringbuffer: Arc::new(RwLock::new(stream_spa_ringbuffer)),
            stream_buffers : Arc::new(RwLock::new(streams_buffers)),
            highwater_mark : Arc::new(RwLock::new(0))
        }
    }
}

impl AudioBackend for PwBackend {
    fn get_stream_state(&self, stream_id: u32) -> Result<u32> {
        let stream_states = self.stream_states.read().unwrap();
        if let Some(state) = stream_states.get(stream_id as usize) {
            Ok(*state)
        } else {
            Err(Error::StreamWithIdNotFound(stream_id))
        }
    }

    fn write(&self, stream_id: u32, req: &mut [u8]) -> Result<u32> {
        self.thread_loop.lock();
        let mut stream_spa_ringbuffer = self.stream_spa_ringbuffer.write().unwrap();
        let ring = stream_spa_ringbuffer.get_mut(stream_id as usize).unwrap();

        let mut stream_buffers = self.stream_buffers.write().unwrap();
        let buffer = stream_buffers.get_mut(stream_id as usize).unwrap();

        let mut index: u32 = 0;

        let mut len : u32 = 0;

        let stream_hash = self.stream_hash.read().unwrap();

        if let Some(stream) = stream_hash.get(&stream_id) {
            if stream.state() != pw::stream::StreamState::Streaming {
                self.thread_loop.unlock();
            } else {
                let filled = unsafe {
                    spa_ringbuffer_get_write_index(ring, &mut index)
                };

                let highwater_mark = *self.highwater_mark.read().unwrap();
                let avail = highwater_mark - filled as u32;

                len = req.len() as u32;

                if len > avail  {
                     len = avail;
                }

                if filled < 0 {
                    println!("underrun write: {} filled: {}", index, filled);
                } else {
                    if filled as u32 + len > RINGBUFFER_SIZE {
                        println!("overrun write: {} filled:{} + size:{} > max:{}",
                        index, filled, len, RINGBUFFER_SIZE);
                    }
                }

                let raw_buffer =  buffer.as_mut_ptr() as *mut c_void;
                let raw_req = req.as_mut_ptr() as *mut c_void;

                unsafe {
                    spa_ringbuffer_write_data(ring, raw_buffer,
                        RINGBUFFER_SIZE, index & RINGBUFFER_MASK, raw_req, len as u32);
                }

                index += len as u32;
                unsafe {
                    spa_ringbuffer_write_update(ring, index);
                }

                self.thread_loop.unlock();
            }
        } else {
            return Err(Error::StreamWithIdNotFound(stream_id));
        };

        //println!("pipewire backend, writting to stream: {}", stream_id);
         Ok(len)
    }

    fn read(&self, _stream_id: u32) -> Result<()> {
        /*
        let buf = req.data_slice().ok_or(Error::SoundReqMissingData)?;
        let zero_mem = vec![0u8; buf.len()];

        buf.copy_from(&zero_mem);
        */
        Ok(())
    }

    fn set_param(&self, stream_id: u32, params: PCMParams) -> Result<()> {
        let mut stream_params = self.stream_params.write().unwrap();
        let mut stream_states = self.stream_states.write().unwrap();
        stream_params[stream_id as usize] = params.clone();

        // TODO: If the device has an intermediate buffer,
        // its size MUST be no less than the specified buffer_bytes value.
        // if u32::from(params.buffer_bytes) > RINGBUFFER_SIZE {
        //     println!("something wrong!");
        // }

        // state {SET_PARAM} -> {SET_PARAM}
        // state {PREPARE} -> {PREPARE}
        if stream_states[stream_id as usize] == 0 {
            // state {0} -> {SET_PARAMS}
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_SET_PARAMS
        }
        Ok(())
    }

    fn prepare(&self, stream_id: u32) -> Result<()> {
        debug!("pipewire backend, prepare function");
        let mut stream_states = self.stream_states.write().unwrap();
        let mut stream_hash = self.stream_hash.write().unwrap();
        let mut stream_listener = self.stream_listener.write().unwrap();
        self.thread_loop.lock();
        let stream_params = self.stream_params.read().unwrap();

        let params = stream_params[stream_id as usize].clone();

        let ring_cloned = self.stream_spa_ringbuffer.clone();

        let buffers_cloned = self.stream_buffers.clone();

        let mut buff = [0; 1024];
        let p_buff = &mut buff as *mut i32 as *mut c_void;

        let mut b = spa_pod_builder {
            data: p_buff,
            size: buff.len() as u32,
            _padding: 0,
            callbacks: spa_callbacks {
                funcs: std::ptr::null(),
                data: std::ptr::null_mut(),
            },
            state: spa_pod_builder_state {
                offset: 0,
                flags: 0,
                frame: std::ptr::null_mut(),
            },
        };

        let mut pos: [u32; 64] = [SPA_AUDIO_CHANNEL_UNKNOWN; 64];

        match params.channels {
            2 => {
                pos[0] = SPA_AUDIO_CHANNEL_FL;
                pos[1] = SPA_AUDIO_CHANNEL_FR;
            }
            1 => {
                pos[0] = SPA_AUDIO_CHANNEL_MONO;
            }
            _ => {
                return Err(Error::ChannelNotSupported);
            }
        }
        let mut info = spa_audio_info_raw {
            format: match params.format {
                VIRTIO_SND_PCM_FMT_S8 => SPA_AUDIO_FORMAT_S8,
                VIRTIO_SND_PCM_FMT_U8 => SPA_AUDIO_FORMAT_U8,
                VIRTIO_SND_PCM_FMT_S16 => SPA_AUDIO_FORMAT_S16,
                VIRTIO_SND_PCM_FMT_U16 => SPA_AUDIO_FORMAT_U16,
                VIRTIO_SND_PCM_FMT_S32 => SPA_AUDIO_FORMAT_S32,
                VIRTIO_SND_PCM_FMT_U32 => SPA_AUDIO_FORMAT_U32,
                _ => SPA_AUDIO_FORMAT_UNKNOWN,
            },
            rate: match params.rate {
                VIRTIO_SND_PCM_RATE_32000 => 32000,
                VIRTIO_SND_PCM_RATE_44100 => 44100,
                VIRTIO_SND_PCM_RATE_48000 => 48000,
                VIRTIO_SND_PCM_RATE_64000 => 64000,
                _ => 44100,
            },
            flags: 0,
            channels: params.channels as u32,
            position: pos,
        };
        let param: *mut spa_pod =
            unsafe { spa_format_audio_raw_build(&mut b, SPA_PARAM_EnumFormat, &mut info) };

        // TODO: to check this 10000??
        let buf_samples: u64 = 10000 * 4 * (info.rate as u64) * 3 / 4 / 1000000;

        let val = format!("{} / {}", buf_samples, info.rate);

        let mut stream = pw::stream::Stream::<i32>::new(
            &self.core,
            "audio-output",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::NODE_LATENCY => val,
            },
        )
        .expect("could not create new stream");
        //     v->highwater_mark = MIN(RINGBUFFER_SIZE,
                //    (ppdo->has_latency ? ppdo->latency : 46440)
                //    * (uint64_t)v->info.rate / 1000000 * v->frame_size);
                //
        let frame_size = info.channels as u32 * size_of::<u16>() as u32;
        let mut highwater_mark = self.highwater_mark.write().unwrap();
        *highwater_mark = cmp::min(RINGBUFFER_SIZE as u32, (46440 * info.rate as u32 / 1000000 * frame_size) as u32);

        if stream_states[stream_id as usize] == VIRTIO_SND_R_PCM_SET_PARAMS
            || stream_states[stream_id as usize] == VIRTIO_SND_R_PCM_RELEASE
        {
            let listener_stream = stream
                .add_local_listener()
                .state_changed(move |old, new| {
                    println!("State changed: {:?} -> {:?}", old, new);
                })
                .param_changed(move |stream, id, _data, param| {
                    if param.is_null() || id != ParamType::Format.as_raw() {
                        return;
                    }
                    let param: *mut spa_pod = unsafe {
                        spa_format_audio_raw_build(&mut b, SPA_PARAM_EnumFormat, &mut info)
                    };

                    //callback to negotiate new set of streams
                    stream
                        .update_params(&mut [param])
                        .expect("could not update params");
                })
                .process(move |stream, _data| {
                    //todo: use safe dequeue_buffer(), contribute queue_buffer()
                    unsafe {
                        let b: *mut pw_buffer = stream.dequeue_raw_buffer();
                        if b.is_null() {
                            return;
                        }
                        let buf = (*b).buffer;
                        let datas = (*buf).datas;
                        let p = (*datas).data;
                        if p.is_null() {
                            return;
                        }

                        // to calculate as sizeof(int16_t) * NR_CHANNELS
                        let frame_size = info.channels as u32 * size_of::<u16>() as u32;
                        let req = (*b).requested * (frame_size as u64);

                        if req == 0{
                            panic!("req = 0");
                        }

                        let mut n_bytes = cmp::min(req as u32, (*datas).maxsize);

                        let buffers_queue = buffers_cloned.read().unwrap();

                        let buffers = buffers_queue.get(stream_id as usize).unwrap();
                        let buffer_ptr = buffers.as_ptr() as *const c_void;

                        let mut index: u32 = 0;
                        let mut ring = ring_cloned.write().unwrap();
                        let ring_mut = ring.get_mut(stream_id as usize).unwrap();

                        //get no of available bytes to read data from buffer
                        let avail = spa_ringbuffer_get_read_index(ring_mut, &mut index);

                        if avail <= 0 {
                            // TODO: to add silent by using
                            // audio_pcm_info_clear_buf(&vo->hw.info, p, n_bytes / v->frame_size);
                            // panic!("avail < 0");
                        } else {
                            if avail < n_bytes as i32 {
                                n_bytes = avail.try_into().unwrap();
                            }
                            spa_ringbuffer_read_data(
                                ring_mut,
                                buffer_ptr,
                                RINGBUFFER_SIZE,
                                index & RINGBUFFER_MASK,
                                p,
                                n_bytes,
                            );
                            index += n_bytes;
                            spa_ringbuffer_read_update(ring_mut, index);
                        }

                        (*(*datas).chunk).offset = 0;
                        (*(*datas).chunk).stride = frame_size as i32;
                        (*(*datas).chunk).size = n_bytes;

                        stream.queue_raw_buffer(b);
                    }
                })
                .register()
                .expect("failed to register stream listener");

            stream_listener.insert(stream_id, listener_stream);

            let direction = match params.direction {
                VIRTIO_SND_D_OUTPUT => spa::Direction::Output,
                VIRTIO_SND_D_INPUT => spa::Direction::Input,
                _ => panic!("Invalid direction"),
            };

            stream
                .connect(
                    direction,
                    Some(pipewire::constants::ID_ANY),
                    pw::stream::StreamFlags::RT_PROCESS
                        | pw::stream::StreamFlags::AUTOCONNECT
                        | pw::stream::StreamFlags::INACTIVE
                        | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut [param],
                )
                .expect("could not connect to the stream");

            self.thread_loop.unlock();
            // insert created stream in a hash table
            stream_hash.insert(stream_id, stream);

            // state {SET_PARAMS} -> {PREPARE}
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_PREPARE;
        } else if stream_states[stream_id as usize] == VIRTIO_SND_R_PCM_PREPARE {
            let stream = stream_hash.get(&stream_id).unwrap();
            stream
                .update_params(&mut [param])
                .expect("could not update params");

            self.thread_loop.unlock();
        }

        Ok(())
    }

    fn release(&self, stream_id: u32) -> Result<()> {
        debug!("pipewire backend, release function");
        self.thread_loop.lock();
        let mut stream_states = self.stream_states.write().unwrap();
        let mut stream_hash = self.stream_hash.write().unwrap();
        let mut stream_listener = self.stream_listener.write().unwrap();

        if let Some(stream) = stream_hash.get(&stream_id) {
            stream.disconnect().expect("could not disconnect stream");
        } else {
            return Err(Error::StreamWithIdNotFound(stream_id));
        };
        stream_hash.remove(&stream_id);
        stream_listener.remove(&stream_id);

        stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_RELEASE;
        self.thread_loop.unlock();
        Ok(())
    }

    fn start(&self, stream_id: u32) -> Result<()> {
        debug!("pipewire start");
        self.thread_loop.lock();
        let mut stream_states = self.stream_states.write().unwrap();
        let stream_hash = self.stream_hash.read().unwrap();
        if let Some(stream) = stream_hash.get(&stream_id) {
            stream.set_active(true).expect("could not start stream");
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_START;
            self.thread_loop.unlock();
            Ok(())
        } else {
            Err(Error::StreamWithIdNotFound(stream_id))
        }
    }

    fn stop(&self, stream_id: u32) -> Result<()> {
        debug!("pipewire stop");
        self.thread_loop.lock();
        let mut stream_states = self.stream_states.write().unwrap();
        let stream_hash = self.stream_hash.read().unwrap();
        if let Some(stream) = stream_hash.get(&stream_id) {
            stream.set_active(false).expect("could not stop stream");
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_STOP;
            self.thread_loop.unlock();
            Ok(())
        } else {
            Err(Error::StreamWithIdNotFound(stream_id))
        }
    }
}
