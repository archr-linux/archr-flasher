//! Minimal FDT (Flattened Device Tree) binary builder for DTBO overlays.
//! Generates valid FDT binaries from scratch — no external tools (no dtc).

use std::collections::HashMap;

const FDT_MAGIC: u32 = 0xd00dfeed;
const FDT_VERSION: u32 = 17;
const FDT_COMPAT_VERSION: u32 = 16;
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_END: u32 = 0x00000009;
const FDT_HEADER_SIZE: u32 = 40;

pub struct FdtBuilder {
    struct_buf: Vec<u8>,
    strings_buf: Vec<u8>,
    strings_map: HashMap<String, u32>,
}

impl FdtBuilder {
    pub fn new() -> Self {
        Self {
            struct_buf: Vec::with_capacity(512),
            strings_buf: Vec::new(),
            strings_map: HashMap::new(),
        }
    }

    pub fn begin_node(&mut self, name: &str) {
        self.put_u32(FDT_BEGIN_NODE);
        self.put_str_aligned(name);
    }

    pub fn end_node(&mut self) {
        self.put_u32(FDT_END_NODE);
    }

    pub fn prop_u32(&mut self, name: &str, val: u32) {
        let nameoff = self.intern_string(name);
        self.put_u32(FDT_PROP);
        self.put_u32(4); // len
        self.put_u32(nameoff);
        self.put_u32(val);
    }

    pub fn prop_str(&mut self, name: &str, val: &str) {
        let nameoff = self.intern_string(name);
        let data = val.as_bytes();
        let len = data.len() as u32 + 1; // include null terminator
        self.put_u32(FDT_PROP);
        self.put_u32(len);
        self.put_u32(nameoff);
        self.struct_buf.extend_from_slice(data);
        self.struct_buf.push(0); // null terminator
        self.align4();
    }

    /// Write a stringlist property (multiple null-terminated strings).
    #[allow(dead_code)]
    pub fn prop_str_list(&mut self, name: &str, vals: &[&str]) {
        let nameoff = self.intern_string(name);
        let mut data = Vec::new();
        for val in vals {
            data.extend_from_slice(val.as_bytes());
            data.push(0);
        }
        let len = data.len() as u32;
        self.put_u32(FDT_PROP);
        self.put_u32(len);
        self.put_u32(nameoff);
        self.struct_buf.extend_from_slice(&data);
        self.align4();
    }

    /// Write raw bytes as a property value.
    pub fn prop_bytes(&mut self, name: &str, data: &[u8]) {
        let nameoff = self.intern_string(name);
        let len = data.len() as u32;
        self.put_u32(FDT_PROP);
        self.put_u32(len);
        self.put_u32(nameoff);
        self.struct_buf.extend_from_slice(data);
        self.align4();
    }

    /// Write an array of u32 values as a property.
    pub fn prop_u32_array(&mut self, name: &str, vals: &[u32]) {
        let nameoff = self.intern_string(name);
        let len = (vals.len() * 4) as u32;
        self.put_u32(FDT_PROP);
        self.put_u32(len);
        self.put_u32(nameoff);
        for v in vals {
            self.put_u32(*v);
        }
    }

    /// Write an empty (boolean) property.
    #[allow(dead_code)]
    pub fn prop_empty(&mut self, name: &str) {
        let nameoff = self.intern_string(name);
        self.put_u32(FDT_PROP);
        self.put_u32(0); // len = 0
        self.put_u32(nameoff);
    }

    /// Build the final FDT binary.
    pub fn finish(mut self) -> Vec<u8> {
        self.put_u32(FDT_END);

        let mem_rsv_size: u32 = 16; // one empty entry (two u64 zeros)
        let off_mem_rsvmap = FDT_HEADER_SIZE;
        let off_dt_struct = off_mem_rsvmap + mem_rsv_size;
        let off_dt_strings = off_dt_struct + self.struct_buf.len() as u32;
        let totalsize = off_dt_strings + self.strings_buf.len() as u32;

        let mut out = Vec::with_capacity(totalsize as usize);

        // Header (40 bytes)
        out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        out.extend_from_slice(&totalsize.to_be_bytes());
        out.extend_from_slice(&off_dt_struct.to_be_bytes());
        out.extend_from_slice(&off_dt_strings.to_be_bytes());
        out.extend_from_slice(&off_mem_rsvmap.to_be_bytes());
        out.extend_from_slice(&FDT_VERSION.to_be_bytes());
        out.extend_from_slice(&FDT_COMPAT_VERSION.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
        out.extend_from_slice(&(self.strings_buf.len() as u32).to_be_bytes());
        out.extend_from_slice(&(self.struct_buf.len() as u32).to_be_bytes());

        // Memory reservation block (empty: 16 bytes of zeros)
        out.extend_from_slice(&[0u8; 16]);

        // Structure block
        out.extend_from_slice(&self.struct_buf);

        // Strings block
        out.extend_from_slice(&self.strings_buf);

        out
    }

    // -- internal helpers --

    fn put_u32(&mut self, val: u32) {
        self.struct_buf.extend_from_slice(&val.to_be_bytes());
    }

    fn put_str_aligned(&mut self, s: &str) {
        self.struct_buf.extend_from_slice(s.as_bytes());
        self.struct_buf.push(0);
        self.align4();
    }

    fn align4(&mut self) {
        let rem = self.struct_buf.len() % 4;
        if rem != 0 {
            for _ in 0..(4 - rem) {
                self.struct_buf.push(0);
            }
        }
    }

    fn intern_string(&mut self, name: &str) -> u32 {
        if let Some(&off) = self.strings_map.get(name) {
            return off;
        }
        let off = self.strings_buf.len() as u32;
        self.strings_buf.extend_from_slice(name.as_bytes());
        self.strings_buf.push(0);
        self.strings_map.insert(name.to_string(), off);
        off
    }
}
