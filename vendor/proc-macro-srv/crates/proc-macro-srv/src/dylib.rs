//! Handles dynamic library loading for proc macro

mod proc_macros;

use rustc_proc_macro::bridge;
use std::{fs, io, time::SystemTime};
use temp_dir::TempDir;

use paths::{Utf8Path, Utf8PathBuf};

use crate::{
    PanicMessage, ProcMacroClientHandle, ProcMacroKind, ProcMacroSrvSpan, TrackedEnv,
    dylib::proc_macros::ProcMacros, token_stream::TokenStream,
};

pub(crate) struct Expander {
    inner: ProcMacroLibrary,
    modified_time: SystemTime,
}

impl Expander {
    pub(crate) fn new(temp_dir: &TempDir, lib: &Utf8Path) -> io::Result<Expander> {
        // Some libraries for dynamic loading require canonicalized path even when it is
        // already absolute
        let lib = lib.canonicalize_utf8()?;
        let modified_time = fs::metadata(&lib).and_then(|it| it.modified())?;

        let path = ensure_file_with_lock_free_access(temp_dir, &lib)?;
        let library = ProcMacroLibrary::open(path.as_ref())?;

        Ok(Expander { inner: library, modified_time })
    }

    pub(crate) fn expand<'a, S: ProcMacroSrvSpan + 'a>(
        &self,
        macro_name: &str,
        macro_body: TokenStream<S>,
        attribute: Option<TokenStream<S>>,
        def_site: S,
        call_site: S,
        mixed_site: S,
        tracked_env: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Result<TokenStream<S>, PanicMessage>
    where
        <S::Server<'a> as bridge::server::Server>::TokenStream: Default,
    {
        self.inner.proc_macros.expand(
            macro_name,
            macro_body,
            attribute,
            def_site,
            call_site,
            mixed_site,
            tracked_env,
            callback,
        )
    }

    pub(crate) fn list_macros(&self) -> impl Iterator<Item = (&str, ProcMacroKind)> {
        self.inner.proc_macros.list_macros()
    }

    pub(crate) fn modified_time(&self) -> SystemTime {
        self.modified_time
    }
}

struct ProcMacroLibrary {
    proc_macros: ProcMacros,
}

/// One entry of the list ra-jit writes into a proc-macro crate it links as a shared library,
/// as `crates/ra-jit/src/macro_list.rs` declares it.
#[repr(C)]
struct Macro {
    kind: u32,
    name: &'static str,
    /// The protocol has no place for a derive's helper attributes: the client reads them
    /// from the source.
    #[allow(dead_code)]
    helpers: &'static [&'static str],
    client: bridge::client::Client,
}

/// The name of the list in the library's symbol table.
const SYMBOL: &std::ffi::CStr = c"RA_JIT_PROC_MACROS";

impl ProcMacroLibrary {
    /// Loads the library and reads ra-jit's list of its macros. The library is never
    /// unloaded, as rustc's loader leaks the libraries it loads: the clients and the names
    /// point into it.
    #[cfg(unix)]
    fn open(path: &Utf8Path) -> io::Result<Self> {
        use std::ffi::{CStr, CString};

        let error = |what: &str| {
            // SAFETY: `dlerror` gives the last error of this thread, or null.
            let said = unsafe { libc::dlerror() };
            let said = match said.is_null() {
                true => "unknown error".into(),
                // SAFETY: a non-null result of `dlerror` is a C string.
                false => unsafe { CStr::from_ptr(said) }.to_string_lossy(),
            };
            io::Error::other(format!("{what} {path}: {said}"))
        };
        let c_path = CString::new(path.as_str()).map_err(io::Error::other)?;
        // SAFETY: loading a library runs its initializers, which a proc-macro library is
        // trusted to have, as rustc trusts it.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(error("cannot load"));
        }
        // SAFETY: the handle is a loaded library's.
        let list = unsafe { libc::dlsym(handle, SYMBOL.as_ptr()) };
        if list.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path} has no list of proc macros: ra-jit did not link it"),
            ));
        }
        // SAFETY: ra-jit exports the list as a `static RA_JIT_PROC_MACROS: &[Macro]`, and the
        // library stays loaded for as long as the process runs.
        let list: &'static [Macro] = unsafe { *list.cast::<&'static [Macro]>() };
        let mut macros = Vec::with_capacity(list.len());
        for mac in list {
            let kind = match mac.kind {
                0 => ProcMacroKind::CustomDerive,
                1 => ProcMacroKind::Attr,
                2 => ProcMacroKind::Bang,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{path}: macro `{}` has unknown kind {other}", mac.name),
                    ));
                }
            };
            macros.push((mac.client, kind, mac.name));
        }

        Ok(ProcMacroLibrary { proc_macros: ProcMacros::new(macros) })
    }

    #[cfg(not(unix))]
    fn open(path: &Utf8Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot load {path}: ra-jit's proc-macro libraries load on unix only"),
        ))
    }
}

/// Copy the dylib to temp directory to prevent locking in Windows
#[cfg(windows)]
fn ensure_file_with_lock_free_access(
    temp_dir: &TempDir,
    path: &Utf8Path,
) -> io::Result<Utf8PathBuf> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    if std::env::var("RA_DONT_COPY_PROC_MACRO_DLL").is_ok() {
        return Ok(path.to_path_buf());
    }

    let mut to = Utf8Path::from_path(temp_dir.path()).unwrap().to_owned();

    let file_name = path.file_stem().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("File path is invalid: {path}"))
    })?;

    to.push({
        // Generate a unique number by abusing `HashMap`'s hasher.
        // Maybe this will also "inspire" a libs team member to finally put `rand` in libstd.
        let unique_name = RandomState::new().build_hasher().finish();
        format!("{file_name}-{unique_name}.dll")
    });
    fs::copy(path, &to)?;
    Ok(to)
}

#[cfg(unix)]
fn ensure_file_with_lock_free_access(
    _temp_dir: &TempDir,
    path: &Utf8Path,
) -> io::Result<Utf8PathBuf> {
    Ok(path.to_owned())
}
