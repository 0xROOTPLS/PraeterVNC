use std::cell::RefCell;
use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};

thread_local! {
    static C: RefCell<Option<(Compressor, i32, Subsamp)>> = const { RefCell::new(None) };
}

pub fn compress(px: &[u32], w: usize, h: usize, quality: u8, ss: Subsamp) -> Vec<u8> {
    C.with(|c| {
        let mut c = c.borrow_mut();
        let st = c.get_or_insert_with(|| (Compressor::new().unwrap(), -1, Subsamp::None));
        if st.1 != quality as i32 {
            st.0.set_quality(quality as i32).unwrap();
            st.1 = quality as i32;
        }
        if st.2 != ss {
            st.0.set_subsamp(ss).unwrap();
            st.2 = ss;
        }
        let bytes = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
        let img = Image { pixels: bytes, width: w, pitch: w * 4, height: h, format: PixelFormat::BGRX };
        st.0.compress_to_vec(img).unwrap()
    })
}
