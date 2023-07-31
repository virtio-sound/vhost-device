// Pipewire backend device
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

use super::AudioBackend;
use crate::vhu_sound::NR_STREAMS;
use crate::virtio_sound::{
    VIRTIO_SND_D_INPUT, VIRTIO_SND_D_OUTPUT, VIRTIO_SND_PCM_FMT_S16, VIRTIO_SND_PCM_FMT_S32,
    VIRTIO_SND_PCM_FMT_S8, VIRTIO_SND_PCM_FMT_U16, VIRTIO_SND_PCM_FMT_U32, VIRTIO_SND_PCM_FMT_U8,
    VIRTIO_SND_PCM_RATE_32000, VIRTIO_SND_PCM_RATE_44100, VIRTIO_SND_PCM_RATE_48000,
    VIRTIO_SND_PCM_RATE_64000, VIRTIO_SND_R_PCM_PREPARE, VIRTIO_SND_R_PCM_SET_PARAMS,
};
use crate::Error;
use crate::PCMParams;
use crate::Result;
use log::error;
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

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::sys::{
    spa_audio_info_raw, spa_callbacks, spa_format_audio_raw_build, spa_pod, spa_pod_builder,
    spa_pod_builder_state, spa_ringbuffer, spa_ringbuffer_get_read_index, spa_ringbuffer_read_data,
    spa_ringbuffer_read_update, SPA_PARAM_EnumFormat, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR,
    SPA_AUDIO_FORMAT_S16, SPA_AUDIO_FORMAT_S32, SPA_AUDIO_FORMAT_S8, SPA_AUDIO_FORMAT_U16,
    SPA_AUDIO_FORMAT_U32, SPA_AUDIO_FORMAT_U8, SPA_AUDIO_FORMAT_UNKNOWN,
};
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

        Self {
            thread_loop,
            core,
            context,
            stream_params: RwLock::new(streams_param),
            stream_states: RwLock::new(streams_states),
            stream_hash: RwLock::new(HashMap::with_capacity(NR_STREAMS)),
            stream_listener: RwLock::new(HashMap::with_capacity(NR_STREAMS)),
        }
    }
}

impl AudioBackend for PwBackend {
    fn get_stream_state(&self, stream_id: u32) -> Result<u32> {
        let stream_states = self.stream_states.read().unwrap();
        dbg!("stream states: {}", stream_states.clone());
        if let Some(state) = stream_states.get(stream_id as usize) {
            Ok(*state)
        } else {
            Err(Error::StreamWithIdNotFound(stream_id))
        }
    }

    fn write(&self, stream_id: u32) -> Result<()> {
        println!("pipewire backend, writting to stream: {}", stream_id);
        Ok(())
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
        stream_params[stream_id as usize] = params;
        // state {SET_PARAM} -> {SET_PARAM}
        // state {PREPARE} -> {PREPARE}
        if stream_states[stream_id as usize] == 0 {
            // state {0} -> {SET_PARAMS}
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_SET_PARAMS
        }
        Ok(())
    }

    fn prepare(&self, stream_id: u32) -> Result<()> {
        println!("pipewire backend, prepare function");
        let mut stream_states = self.stream_states.write().unwrap();
        let mut stream_hash = self.stream_hash.write().unwrap();
        let mut stream_listener = self.stream_listener.write().unwrap();
        self.thread_loop.lock();
        let stream_params = self.stream_params.read().unwrap();
        let params = stream_params[stream_id as usize].clone();

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

        let mut pos: [u32; 64] = [0; 64];

        //todo map position to chmap
        pos[0] = SPA_AUDIO_CHANNEL_FL;
        pos[1] = SPA_AUDIO_CHANNEL_FR;
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

        let mut stream = pw::stream::Stream::<i32>::new(
            &self.core,
            "audio-output",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
            },
        )
        .expect("could not create new stream");

        if stream_states[stream_id as usize] == VIRTIO_SND_R_PCM_SET_PARAMS {
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

                        let frame_size = info.channels;
                        let req = (*b).requested * (frame_size as u64);
                        let mut n_bytes = cmp::min(req as u32, (*datas).maxsize);

                        let buffer: Vec<u8> = vec![0; RINGBUFFER_SIZE as usize];
                        let buffer_ptr = buffer.as_ptr() as *const c_void;

                        //use spa_ringbuffer functions
                        let mut index: u32 = 0;
                        let mut ring = spa_ringbuffer {
                            readindex: 0,
                            writeindex: 0,
                        };
                        //get no of available bytes to read data from buffer
                        let avail = spa_ringbuffer_get_read_index(&mut ring, &mut index);
                        if avail < n_bytes as i32 {
                            n_bytes = avail.try_into().unwrap();
                        }
                        spa_ringbuffer_read_data(
                            &mut ring,
                            buffer_ptr,
                            RINGBUFFER_SIZE,
                            index & RINGBUFFER_MASK,
                            p,
                            n_bytes,
                        );
                        index += n_bytes;
                        spa_ringbuffer_read_update(&mut ring, index);

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
        println!("pipewire backend, release function");
        self.thread_loop.lock();
        let mut stream_states = self.stream_states.write().unwrap();
        let stream_hash = self.stream_hash.read().unwrap();
        let stream_listener = self.stream_listener.read().unwrap();

        if let Some(stream) = stream_hash.get(&stream_id) {
            stream.disconnect().expect("could not disconnect stream");
        } else {
            error!("stream not found for the given stream_id");
        };
        if let Some(stream_lis) = stream_listener.get(&stream_id) {
            //unregister stream
            std::mem::drop(stream_lis);
        } else {
            error!("stream listener not found for the given stream_id");
        };

        if stream_states[stream_id as usize] == VIRTIO_SND_R_PCM_PREPARE {
            stream_states[stream_id as usize] = VIRTIO_SND_R_PCM_SET_PARAMS;
        }
        self.thread_loop.unlock();
        Ok(())
    }
}
