use std::cell::RefCell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioBuffer, AudioBufferSourceNode, AudioContext, GainNode};

#[wasm_bindgen(start)]
pub fn start() { console_error_panic_hook::set_once(); }

thread_local! {
    static CTX:   RefCell<Option<AudioContext>> = RefCell::new(None);
    static C4BUF: RefCell<Option<AudioBuffer>>  = RefCell::new(None);
    static READY: RefCell<bool>                 = RefCell::new(false);
}

fn ratio(semi: i32) -> f32 { (2f32).powf(semi as f32 / 12.0) }
const K17: [i32; 17] = [0,2,4,5,7,9,11,12,14,16,17,19,21,23,24,26,28];

#[wasm_bindgen]
pub async fn init_audio() -> Result<(), JsValue> {
    if READY.with(|r| *r.borrow()) { return Ok(()); }

    let win = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let ctx = AudioContext::new()?;

    // キャッシュ無効化して取得
    let mut init = web_sys::RequestInit::new();
    init.method("GET");
    init.cache(web_sys::RequestCache::NoStore);
    let url = format!("./assets/c4.wav?ts={}", js_sys::Date::now());
    let req = web_sys::Request::new_with_str_and_init(&url, &init)?;

    let resp_val = JsFuture::from(win.fetch_with_request(&req)).await?;
    let resp: web_sys::Response = resp_val.dyn_into()?;
    if !(200..=299).contains(&resp.status()) {
        return Err(JsValue::from_str(&format!("fetch failed: status {}", resp.status())));
    }
    let abuf = JsFuture::from(resp.array_buffer()?).await?.dyn_into::<js_sys::ArrayBuffer>()?;

    // ---- ここから: PCM16 WAV を自前でパース ----
    let (channels, sample_rate, frames, interleaved_i16, data_offset) = parse_wav_pcm16(&abuf)
        .map_err(|e| JsValue::from_str(e))?;

    // AudioBuffer を作り、Int16 を [-1,1] の f32 に直して流し込む
    let buffer = ctx.create_buffer(channels as u32, frames as u32, sample_rate as f32)?;
    for ch in 0..channels {
        let mut mono = vec![0f32; frames as usize];
        let mut i = ch as usize;
        let step = channels as usize;

        for frame in 0..frames as usize {
            mono[frame] = interleaved_i16[i] as f32 / 32768.0;
            i += step;
        }

        buffer.copy_to_channel(&mut mono[..], ch as i32)?;
    }
    // ---- ここまで ----

    // 保存
    CTX.with(|c| *c.borrow_mut() = Some(ctx));
    C4BUF.with(|b| *b.borrow_mut() = Some(buffer));
    READY.with(|r| *r.borrow_mut() = true);
    web_sys::console::log_1(&JsValue::from_str(&format!("WAV OK (ch={}, sr={}, frames={}, data@{})",
        channels, sample_rate, frames, data_offset)));
    Ok(())
}

#[wasm_bindgen]
pub fn note_on(index: usize, velocity: f32) -> Result<(), JsValue> {
    if !READY.with(|r| *r.borrow()) { return Err(JsValue::from_str("Audio not initialized")); }
    CTX.with(|c| {
        let ctx = c.borrow();
        let ctx = ctx.as_ref().ok_or_else(|| JsValue::from_str("no ctx"))?;
        C4BUF.with(|b| {
            let base = b.borrow();
            let base = base.as_ref().ok_or_else(|| JsValue::from_str("no buffer"))?;
            let src: AudioBufferSourceNode = ctx.create_buffer_source()?;
            src.set_buffer(Some(base));
            src.playback_rate().set_value(ratio(K17[index.min(16)]));
            let gain: GainNode = ctx.create_gain()?;
            gain.gain().set_value(velocity.clamp(0.0, 1.0));
            src.connect_with_audio_node(&gain)?;
            gain.connect_with_audio_node(&ctx.destination())?;
            src.start()?;
            Ok(())
        })
    })
}

/// PCM16 WAV だけを読み取る簡易パーサ
fn parse_wav_pcm16(abuf: &js_sys::ArrayBuffer)
    -> Result<(u16 /*channels*/, u32 /*sr*/, u32 /*frames*/, Vec<i16> /*interleaved*/, usize /*data_off*/),
              &'static str>
{
    let u8 = js_sys::Uint8Array::new(abuf);
    let len = u8.length() as usize;
    if len < 44 { return Err("too small"); }

    // 4字 + size + 4字
    if &u8.slice(0,4).to_vec()[..] != b"RIFF" { return Err("not RIFF"); }
    if &u8.slice(8,12).to_vec()[..] != b"WAVE" { return Err("not WAVE"); }

    // チャンクを走査
    let mut off = 12usize;
    let mut fmt_found = false;
    let mut data_off = 0usize;
    let mut data_size = 0usize;

    // fmt の情報
    let mut audio_format = 0u16;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut block_align = 0u16;

    while off + 8 <= len {
        let id = u8.slice(off as u32, (off+4) as u32).to_vec();
        let sz = le_u32(&u8, off+4)? as usize;
        let next = off + 8 + sz;
        match &id[..] {
            b"fmt " => {
                if sz < 16 { return Err("fmt too small"); }
                audio_format    = le_u16(&u8, off+8)?;
                channels        = le_u16(&u8, off+10)?;
                sample_rate     = le_u32(&u8, off+12)?;
                block_align     = le_u16(&u8, off+20)?;
                bits_per_sample = le_u16(&u8, off+22)?;
                fmt_found = true;
            }
            b"data" => {
                data_off = off + 8;
                data_size = sz;
            }
            _ => {}
        }
        off = next + (sz & 1); // パディング考慮
        if off > len { break; }
    }

    if !fmt_found { return Err("fmt chunk not found"); }
    if data_off == 0 { return Err("data chunk not found"); }
    if audio_format != 1 { return Err("not PCM (format != 1)"); }
    if bits_per_sample != 16 { return Err("not 16-bit PCM"); }
    if channels == 0 { return Err("channels=0"); }
    if block_align as usize != (channels as usize * 2) { return Err("bad block_align"); }
    if data_off + data_size > len { return Err("bad data size"); }

    let frames = (data_size / block_align as usize) as u32;

    // i16 に変換（LE）
    let mut out = Vec::<i16>::with_capacity((data_size/2) as usize);
    let mut p = data_off;
    while p + 1 < data_off + data_size {
        let lo = u8.get_index(p as u32) as u16;
        let hi = u8.get_index((p+1) as u32) as u16;
        let val = ((hi << 8) | lo) as i16;
        out.push(val);
        p += 2;
    }

    Ok((channels, sample_rate, frames, out, data_off))
}

fn le_u16(u8: &js_sys::Uint8Array, off: usize) -> Result<u16, &'static str> {
    if off+1 >= u8.length() as usize { return Err("oob"); }
    Ok(((u8.get_index((off+1) as u32) as u16) << 8) | (u8.get_index(off as u32) as u16))
}
fn le_u32(u8: &js_sys::Uint8Array, off: usize) -> Result<u32, &'static str> {
    if off+3 >= u8.length() as usize { return Err("oob"); }
    Ok(((u8.get_index((off+3) as u32) as u32) << 24)
      |((u8.get_index((off+2) as u32) as u32) << 16)
      |((u8.get_index((off+1) as u32) as u32) << 8)
      | (u8.get_index(off as u32) as u32))
}
