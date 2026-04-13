//! WASM binary parser — extracts sections needed for JIT compilation.
//!
//! Parses only what silverfir needs: type section, function section,
//! export section, code section. Ignores everything else.
//!
//! Reference: WebAssembly spec §5.5 (Binary Format)

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// WASM magic number and version
const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6D]; // \0asm
const WASM_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

/// Section IDs
const SEC_TYPE: u8 = 1;
const SEC_IMPORT: u8 = 2;
const SEC_FUNCTION: u8 = 3;
const SEC_EXPORT: u8 = 7;
const SEC_CODE: u8 = 10;

/// Parsed WASM module
pub struct WasmModule {
    /// Function type signatures: (params, results)
    pub types: Vec<FuncType>,
    /// Function index → type index mapping
    pub func_type_indices: Vec<u32>,
    /// Exports: (name, kind, index)
    pub exports: Vec<Export>,
    /// Code bodies: (locals, bytecode)
    pub code_bodies: Vec<CodeBody>,
    /// Number of imported functions (offsets func indices)
    pub num_imports: u32,
}

#[derive(Clone)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
}

pub struct Export {
    pub name: String,
    pub kind: u8, // 0=func, 1=table, 2=memory, 3=global
    pub index: u32,
}

pub struct CodeBody {
    pub locals: Vec<(u32, ValType)>, // (count, type) pairs
    pub bytecode: Vec<u8>,
}

/// Parse error
pub enum ParseError {
    InvalidMagic,
    InvalidVersion,
    UnexpectedEof,
    InvalidSection(u8),
    InvalidEncoding(String),
}

impl WasmModule {
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        let mut pos = 0;

        // Magic + version
        if data.len() < 8 { return Err(ParseError::UnexpectedEof); }
        if data[0..4] != WASM_MAGIC { return Err(ParseError::InvalidMagic); }
        if data[4..8] != WASM_VERSION { return Err(ParseError::InvalidVersion); }
        pos = 8;

        let mut module = WasmModule {
            types: Vec::new(),
            func_type_indices: Vec::new(),
            exports: Vec::new(),
            code_bodies: Vec::new(),
            num_imports: 0,
        };

        // Parse sections
        while pos < data.len() {
            let section_id = data[pos]; pos += 1;
            let (section_len, n) = read_leb128_u32(&data[pos..])?;
            pos += n;
            let section_end = pos + section_len as usize;
            if section_end > data.len() { return Err(ParseError::UnexpectedEof); }

            match section_id {
                SEC_TYPE => parse_type_section(&data[pos..section_end], &mut module)?,
                SEC_IMPORT => parse_import_section(&data[pos..section_end], &mut module)?,
                SEC_FUNCTION => parse_function_section(&data[pos..section_end], &mut module)?,
                SEC_EXPORT => parse_export_section(&data[pos..section_end], &mut module)?,
                SEC_CODE => parse_code_section(&data[pos..section_end], &mut module)?,
                _ => {} // skip unknown sections
            }
            pos = section_end;
        }

        Ok(module)
    }

    /// Find the code body index for an exported function by name.
    pub fn find_export_func(&self, name: &str) -> Option<usize> {
        for exp in &self.exports {
            if exp.kind == 0 && exp.name == name {
                // Exported func index includes imports
                let local_idx = exp.index.checked_sub(self.num_imports)?;
                return Some(local_idx as usize);
            }
        }
        None
    }
}

// ── Section parsers ─────────────────────────────────────────────────

fn parse_type_section(data: &[u8], module: &mut WasmModule) -> Result<(), ParseError> {
    let mut pos = 0;
    let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;

    for _ in 0..count {
        if data[pos] != 0x60 { return Err(ParseError::InvalidEncoding(String::from("expected functype 0x60"))); }
        pos += 1;

        // Params
        let (param_count, n) = read_leb128_u32(&data[pos..])?; pos += n;
        let mut params = Vec::new();
        for _ in 0..param_count {
            params.push(read_valtype(data[pos])?);
            pos += 1;
        }

        // Results
        let (result_count, n) = read_leb128_u32(&data[pos..])?; pos += n;
        let mut results = Vec::new();
        for _ in 0..result_count {
            results.push(read_valtype(data[pos])?);
            pos += 1;
        }

        module.types.push(FuncType { params, results });
    }
    Ok(())
}

fn parse_import_section(data: &[u8], module: &mut WasmModule) -> Result<(), ParseError> {
    let mut pos = 0;
    let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;

    for _ in 0..count {
        // Skip module name
        let (mod_len, n) = read_leb128_u32(&data[pos..])?; pos += n;
        pos += mod_len as usize;
        // Skip field name
        let (field_len, n) = read_leb128_u32(&data[pos..])?; pos += n;
        pos += field_len as usize;
        // Import kind
        let kind = data[pos]; pos += 1;
        match kind {
            0x00 => { // func import
                let (_type_idx, n) = read_leb128_u32(&data[pos..])?; pos += n;
                module.num_imports += 1;
            }
            0x01 => { pos += 2; } // table: skip elemtype + limits
            0x02 => { // memory: skip limits
                let flags = data[pos]; pos += 1;
                let (_min, n) = read_leb128_u32(&data[pos..])?; pos += n;
                if flags & 1 != 0 { let (_, n) = read_leb128_u32(&data[pos..])?; pos += n; }
            }
            0x03 => { pos += 2; } // global: skip valtype + mutability
            _ => return Err(ParseError::InvalidEncoding(String::from("bad import kind"))),
        }
    }
    Ok(())
}

fn parse_function_section(data: &[u8], module: &mut WasmModule) -> Result<(), ParseError> {
    let mut pos = 0;
    let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;

    for _ in 0..count {
        let (type_idx, n) = read_leb128_u32(&data[pos..])?; pos += n;
        module.func_type_indices.push(type_idx);
    }
    Ok(())
}

fn parse_export_section(data: &[u8], module: &mut WasmModule) -> Result<(), ParseError> {
    let mut pos = 0;
    let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;

    for _ in 0..count {
        let (name_len, n) = read_leb128_u32(&data[pos..])?; pos += n;
        let name = core::str::from_utf8(&data[pos..pos + name_len as usize])
            .map_err(|_| ParseError::InvalidEncoding(String::from("bad export name")))?;
        pos += name_len as usize;
        let kind = data[pos]; pos += 1;
        let (index, n) = read_leb128_u32(&data[pos..])?; pos += n;

        module.exports.push(Export {
            name: String::from(name),
            kind,
            index,
        });
    }
    Ok(())
}

fn parse_code_section(data: &[u8], module: &mut WasmModule) -> Result<(), ParseError> {
    let mut pos = 0;
    let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;

    for _ in 0..count {
        let (body_size, n) = read_leb128_u32(&data[pos..])?; pos += n;
        let body_end = pos + body_size as usize;

        // Parse locals
        let (local_count, n) = read_leb128_u32(&data[pos..])?; pos += n;
        let mut locals = Vec::new();
        for _ in 0..local_count {
            let (count, n) = read_leb128_u32(&data[pos..])?; pos += n;
            let vtype = read_valtype(data[pos])?; pos += 1;
            locals.push((count, vtype));
        }

        // Rest is bytecode (up to 0x0B end marker)
        let bytecode = Vec::from(&data[pos..body_end]);
        pos = body_end;

        module.code_bodies.push(CodeBody { locals, bytecode });
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

fn read_valtype(byte: u8) -> Result<ValType, ParseError> {
    match byte {
        0x7F => Ok(ValType::I32),
        0x7E => Ok(ValType::I64),
        0x7D => Ok(ValType::F32),
        0x7C => Ok(ValType::F64),
        _ => Err(ParseError::InvalidEncoding(String::from("bad valtype"))),
    }
}

/// Read unsigned LEB128. Returns (value, bytes_consumed).
fn read_leb128_u32(data: &[u8]) -> Result<(u32, usize), ParseError> {
    let mut result: u32 = 0;
    let mut shift = 0;
    for i in 0..5 {
        if i >= data.len() { return Err(ParseError::UnexpectedEof); }
        let byte = data[i];
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    Err(ParseError::InvalidEncoding(String::from("LEB128 too long")))
}

/// Read signed LEB128 as i32.
pub fn read_leb128_i32(data: &[u8]) -> Result<(i32, usize), ParseError> {
    let mut result: i32 = 0;
    let mut shift = 0;
    for i in 0..5 {
        if i >= data.len() { return Err(ParseError::UnexpectedEof); }
        let byte = data[i];
        result |= ((byte & 0x7F) as i32) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 32 && (byte & 0x40) != 0 {
                result |= !0 << shift; // sign extend
            }
            return Ok((result, i + 1));
        }
    }
    Err(ParseError::InvalidEncoding(String::from("LEB128 too long")))
}
