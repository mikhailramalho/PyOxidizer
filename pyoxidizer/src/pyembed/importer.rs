// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/* This module defines a Python meta path importer for importing from a self-contained binary. */

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::io::Cursor;

use byteorder::{LittleEndian, ReadBytesExt};
use cpython::exc::{ImportError, RuntimeError, ValueError};
use cpython::{
    py_class, py_class_impl, py_coerce_item, py_fn, NoArgs, ObjectProtocol, PyDict, PyErr, PyList,
    PyModule, PyObject, PyResult, PyString, Python, PythonObject,
};
use python3_sys as pyffi;
use python3_sys::{PyBUF_READ, PyMemoryView_FromMemory};

use super::pyinterp::PYOXIDIZER_IMPORTER_NAME;

/// Obtain a Python memoryview referencing a memory slice.
///
/// New memoryview allows Python to access the underlying memory without
/// copying it.
#[inline]
fn get_memory_view(py: Python, data: &'static [u8]) -> Option<PyObject> {
    let ptr = unsafe { PyMemoryView_FromMemory(data.as_ptr() as _, data.len() as _, PyBUF_READ) };
    unsafe { PyObject::from_owned_ptr_opt(py, ptr) }
}

/// Represents Python modules data in memory.
///
/// That data can be source, bytecode, etc. This type is just a thin wrapper around
/// a mapping of module name to blob data.
struct PythonModulesData {
    data: HashMap<&'static str, &'static [u8]>,
}

impl PythonModulesData {
    /// Obtain a PyMemoryView instance for a specific key.
    fn get_memory_view(&self, py: Python, name: &str) -> Option<PyObject> {
        match self.data.get(name) {
            Some(value) => get_memory_view(py, value),
            None => None,
        }
    }
}

/// Parse modules blob data into a map of module name to module data.
fn parse_modules_blob(data: &'static [u8]) -> Result<HashMap<&str, &[u8]>, &'static str> {
    if data.len() < 4 {
        return Err("modules data too small");
    }

    let mut reader = Cursor::new(data);

    let count = reader.read_u32::<LittleEndian>().unwrap();
    let mut index = Vec::with_capacity(count as usize);
    let mut total_names_length = 0;

    let mut i = 0;
    while i < count {
        let name_length = reader.read_u32::<LittleEndian>().unwrap() as usize;
        let data_length = reader.read_u32::<LittleEndian>().unwrap() as usize;

        index.push((name_length, data_length));
        total_names_length += name_length;
        i += 1;
    }

    let mut res = HashMap::with_capacity(count as usize);
    let values_start_offset = reader.position() as usize + total_names_length;
    let mut values_current_offset: usize = 0;

    for (name_length, value_length) in index {
        let offset = reader.position() as usize;

        let name = unsafe { std::str::from_utf8_unchecked(&data[offset..offset + name_length]) };

        let value_offset = values_start_offset + values_current_offset;
        let value = &data[value_offset..value_offset + value_length];
        reader.set_position(offset as u64 + name_length as u64);
        values_current_offset += value_length;

        res.insert(name, value);
    }

    Ok(res)
}

#[allow(unused_doc_comments)]
/// Python type to import modules.
///
/// This type implements the importlib.abc.MetaPathFinder interface for
/// finding/loading modules. It supports loading various flavors of modules,
/// allowing it to be the only registered sys.meta_path importer.
py_class!(class PyOxidizerFinder |py| {
    data imp_module: PyModule;
    data marshal_loads: PyObject;
    data builtin_importer: PyObject;
    data frozen_importer: PyObject;
    data call_with_frames_removed: PyObject;
    data module_spec_type: PyObject;
    data decode_source: PyObject;
    data exec_fn: PyObject;
    data py_modules: PythonModulesData;
    data pyc_modules: PythonModulesData;
    data packages: HashSet<&'static str>;
    data known_modules: KnownModules;

    // Start of importlib.abc.MetaPathFinder interface.

    def find_spec(&self, fullname: &PyString, path: &PyObject, target: Option<PyObject> = None) -> PyResult<PyObject> {
        let key = fullname.to_string(py)?;

        if let Some(flavor) = self.known_modules(py).get(&*key) {
            match flavor {
                KnownModuleFlavor::Builtin => {
                    self.builtin_importer(py).call_method(py, "find_spec", (fullname, path, target), None)
                }
                KnownModuleFlavor::Frozen => {
                    self.frozen_importer(py).call_method(py, "find_spec", (fullname, path, target), None)
                }
                KnownModuleFlavor::InMemory => {
                    let is_package = self.packages(py).contains(&*key);

                    // TODO consider setting origin and has_location so __file__ will be
                    // populated.

                    let kwargs = PyDict::new(py);
                    kwargs.set_item(py, "is_package", is_package)?;

                    self.module_spec_type(py).call(py, (fullname, self), Some(&kwargs))
                }
            }
        } else {
            Ok(py.None())
        }
    }

    def find_module(&self, _fullname: &PyObject, _path: &PyObject) -> PyResult<PyObject> {
        // Method is deprecated. Always returns None.
        // We /could/ call find_spec(). Meh.
        Ok(py.None())
    }

    def invalidate_caches(&self) -> PyResult<PyObject> {
        Ok(py.None())
    }

    // End of importlib.abc.MetaPathFinder interface.

    // Start of importlib.abc.Loader interface.

    def create_module(&self, _spec: &PyObject) -> PyResult<PyObject> {
        Ok(py.None())
    }

    def exec_module(&self, module: &PyObject) -> PyResult<PyObject> {
        let name = module.getattr(py, "__name__")?;
        let key = name.extract::<String>(py)?;

        if let Some(flavor) = self.known_modules(py).get(&*key) {
            match flavor {
                KnownModuleFlavor::Builtin => {
                    self.builtin_importer(py).call_method(py, "exec_module", (module,), None)
                },
                KnownModuleFlavor::Frozen => {
                    self.frozen_importer(py).call_method(py, "exec_module", (module,), None)
                },
                KnownModuleFlavor::InMemory => {
                    match self.pyc_modules(py).get_memory_view(py, &*key) {
                        Some(value) => {
                            let code = self.marshal_loads(py).call(py, (value,), None)?;
                            let exec_fn = self.exec_fn(py);
                            let dict = module.getattr(py, "__dict__")?;

                            self.call_with_frames_removed(py).call(py, (exec_fn, code, dict), None)
                        },
                        None => {
                            Err(PyErr::new::<ImportError, _>(py, ("cannot find code in memory", name)))
                        }
                    }
                },
            }
        } else {
            // Raising here might make more sense, as exec_module() shouldn't
            // be called on the Loader that didn't create the module.
            Ok(py.None())
        }
    }

    // End of importlib.abc.Loader interface.

    // Start of importlib.abc.InspectLoader interface.

    def get_code(&self, fullname: &PyString) -> PyResult<PyObject> {
        let key = fullname.to_string(py)?;

        if let Some(flavor) = self.known_modules(py).get(&*key) {
            match flavor {
                KnownModuleFlavor::Frozen => {
                    let imp_module = self.imp_module(py);

                    imp_module.call(py, "get_frozen_object", (fullname,), None)
                },
                KnownModuleFlavor::InMemory => {
                    match self.pyc_modules(py).get_memory_view(py, &*key) {
                        Some(value) => {
                            self.marshal_loads(py).call(py, (value,), None)
                        }
                        None => {
                            Err(PyErr::new::<ImportError, _>(py, ("cannot find code in memory", fullname)))
                        }
                    }
                },
                KnownModuleFlavor::Builtin => {
                    Ok(py.None())
                }
            }
        } else {
            Ok(py.None())
        }
    }

    def get_source(&self, fullname: &PyString) -> PyResult<PyObject> {
        let key = fullname.to_string(py)?;

        if let Some(flavor) = self.known_modules(py).get(&*key) {
            if let KnownModuleFlavor::InMemory = flavor {
                match self.py_modules(py).get_memory_view(py, &*key) {
                    Some(value) => {
                        self.decode_source(py).call(py, (value,), None)
                    },
                    None => {
                        Err(PyErr::new::<ImportError, _>(py, ("source not available", fullname)))
                    }
                }
            } else {
                Ok(py.None())
            }
        } else {
            Ok(py.None())
        }
    }

    // End of importlib.abc.InspectLoader interface.
});

fn populate_packages(packages: &mut HashSet<&'static str>, name: &'static str) {
    let mut search = name;

    while let Some(idx) = search.rfind('.') {
        packages.insert(&search[0..idx]);
        search = &search[0..idx];
    }
}

const DOC: &[u8] = b"Binary representation of Python modules\0";

/// Represents global module state to be passed at interpreter initialization time.
#[derive(Debug)]
pub struct InitModuleState {
    /// Raw data constituting Python module source code.
    pub py_data: &'static [u8],

    /// Raw data constituting Python module bytecode.
    pub pyc_data: &'static [u8],
}

/// Holds reference to next module state struct.
///
/// This module state will be copied into the module's state when the
/// Python module is initialized.
pub static mut NEXT_MODULE_STATE: *const InitModuleState = std::ptr::null();

/// Represents which importer to use for known modules.
#[derive(Debug)]
enum KnownModuleFlavor {
    Builtin,
    Frozen,
    InMemory,
}

type KnownModules = HashMap<&'static str, KnownModuleFlavor>;

/// State associated with each importer module instance.
///
/// We write per-module state to per-module instances of this struct so
/// we don't rely on global variables and so multiple importer modules can
/// exist without issue.
#[derive(Debug)]
struct ModuleState {
    /// Raw data constituting Python module source code.
    py_data: &'static [u8],

    /// Raw data constituting Python module bytecode.
    pyc_data: &'static [u8],

    /// Whether setup() has been called.
    setup_called: bool,
}

/// Obtain the module state for an instance of our importer module.
///
/// Creates a Python exception on failure.
///
/// Doesn't do type checking that the PyModule is of the appropriate type.
fn get_module_state<'a>(py: Python, m: &'a PyModule) -> Result<&'a mut ModuleState, PyErr> {
    let ptr = m.as_object().as_ptr();
    let state = unsafe { pyffi::PyModule_GetState(ptr) as *mut ModuleState };

    if state.is_null() {
        let err = PyErr::new::<ValueError, _>(py, "unable to retrieve module state");
        return Err(err);
    }

    Ok(unsafe { &mut *state })
}

static mut MODULE_DEF: pyffi::PyModuleDef = pyffi::PyModuleDef {
    m_base: pyffi::PyModuleDef_HEAD_INIT,
    m_name: PYOXIDIZER_IMPORTER_NAME.as_ptr() as *const _,
    m_doc: DOC.as_ptr() as *const _,
    m_size: std::mem::size_of::<ModuleState>() as isize,
    m_methods: 0 as *mut _,
    m_slots: 0 as *mut _,
    m_traverse: None,
    m_clear: None,
    m_free: None,
};

/// Initialize the Python module object.
///
/// This is called as part of the PyInit_* function to create the internal
/// module object for the interpreter.
///
/// This receives a handle to the current Python interpreter and just-created
/// Python module instance. It populates the internal module state and registers
/// a _setup() on the module object for usage by Python.
///
/// Because this function accesses NEXT_MODULE_STATE, it should only be
/// called during interpreter initialization.
fn module_init(py: Python, m: &PyModule) -> PyResult<()> {
    let mut state = get_module_state(py, m)?;

    unsafe {
        state.py_data = (*NEXT_MODULE_STATE).py_data;
        state.pyc_data = (*NEXT_MODULE_STATE).pyc_data;
    }

    state.setup_called = false;

    m.add(
        py,
        "_setup",
        py_fn!(
            py,
            module_setup(
                m: PyModule,
                bootstrap_module: PyModule,
                marshal_module: PyModule,
                decode_source: PyObject
            )
        ),
    )?;

    Ok(())
}

/// Called after module import/initialization to configure the importing mechanism.
///
/// This does the heavy work of configuring the importing mechanism.
///
/// This function should only be called once as part of
/// _frozen_importlib_external._install_external_importers().
fn module_setup(
    py: Python,
    m: PyModule,
    bootstrap_module: PyModule,
    marshal_module: PyModule,
    decode_source: PyObject,
) -> PyResult<PyObject> {
    let state = get_module_state(py, &m)?;

    if state.setup_called {
        return Err(PyErr::new::<RuntimeError, _>(
            py,
            "PyOxidizer _setup() already called",
        ));
    }

    state.setup_called = true;

    let imp_module = bootstrap_module.get(py, "_imp")?;
    let imp_module = imp_module.cast_into::<PyModule>(py)?;
    let sys_module = bootstrap_module.get(py, "sys")?;
    let sys_module = sys_module.cast_as::<PyModule>(py)?;
    let meta_path_object = sys_module.get(py, "meta_path")?;

    // We should be executing as part of
    // _frozen_importlib_external._install_external_importers().
    // _frozen_importlib._install() should have already been called and set up
    // sys.meta_path with [BuiltinImporter, FrozenImporter]. Those should be the
    // only meta path importers present.

    let meta_path = meta_path_object.cast_as::<PyList>(py)?;

    if meta_path.len(py) != 2 {
        return Err(PyErr::new::<ValueError, _>(
            py,
            "sys.meta_path does not contain 2 values",
        ));
    }

    let builtin_importer = meta_path.get_item(py, 0);
    let frozen_importer = meta_path.get_item(py, 1);

    let py_modules = match parse_modules_blob(state.py_data) {
        Ok(value) => value,
        Err(msg) => return Err(PyErr::new::<ValueError, _>(py, msg)),
    };

    let pyc_modules = match parse_modules_blob(state.pyc_data) {
        Ok(value) => value,
        Err(msg) => return Err(PyErr::new::<ValueError, _>(py, msg)),
    };

    // Populate our known module lookup table with entries from builtins, frozens, and
    // finally us. Last write wins and has the same effect as registering our
    // meta path importer first. This should be safe. If nothing else, it allows
    // some builtins to be overwritten by .py implemented modules.
    let mut known_modules = KnownModules::with_capacity(pyc_modules.len() + 10);

    for i in 0.. {
        let record = unsafe { pyffi::PyImport_Inittab.offset(i) };

        if unsafe { *record }.name.is_null() {
            break;
        }

        let name = unsafe { CStr::from_ptr((*record).name as _) };
        let name_str = match name.to_str() {
            Ok(v) => v,
            Err(_) => {
                return Err(PyErr::new::<ValueError, _>(
                    py,
                    "unable to parse PyImport_Inittab",
                ));
            }
        };

        known_modules.insert(name_str, KnownModuleFlavor::Builtin);
    }

    for i in 0.. {
        let record = unsafe { pyffi::PyImport_FrozenModules.offset(i) };

        if unsafe { *record }.name.is_null() {
            break;
        }

        let name = unsafe { CStr::from_ptr((*record).name as _) };
        let name_str = match name.to_str() {
            Ok(v) => v,
            Err(_) => {
                return Err(PyErr::new::<ValueError, _>(
                    py,
                    "unable to parse PyImport_FrozenModules",
                ));
            }
        };

        known_modules.insert(name_str, KnownModuleFlavor::Frozen);
    }

    // TODO consider baking set of packages into embedded data.
    let mut packages: HashSet<&'static str> = HashSet::with_capacity(pyc_modules.len());

    for key in py_modules.keys() {
        known_modules.insert(key, KnownModuleFlavor::InMemory);
        populate_packages(&mut packages, key);
    }

    for key in pyc_modules.keys() {
        known_modules.insert(key, KnownModuleFlavor::InMemory);
        populate_packages(&mut packages, key);
    }

    let marshal_loads = marshal_module.get(py, "loads")?;
    let call_with_frames_removed = bootstrap_module.get(py, "_call_with_frames_removed")?;
    let module_spec_type = bootstrap_module.get(py, "ModuleSpec")?;

    let builtins_module =
        match unsafe { PyObject::from_borrowed_ptr_opt(py, pyffi::PyEval_GetBuiltins()) } {
            Some(o) => o.cast_into::<PyDict>(py),
            None => {
                return Err(PyErr::new::<ValueError, _>(
                    py,
                    "unable to obtain __builtins__",
                ));
            }
        }?;

    let exec_fn = match builtins_module.get_item(py, "exec") {
        Some(v) => v,
        None => {
            return Err(PyErr::new::<ValueError, _>(
                py,
                "could not obtain __builtins__.exec",
            ));
        }
    };

    let unified_importer = PyOxidizerFinder::create_instance(
        py,
        imp_module,
        marshal_loads,
        builtin_importer,
        frozen_importer,
        call_with_frames_removed,
        module_spec_type,
        decode_source,
        exec_fn,
        PythonModulesData { data: py_modules },
        PythonModulesData { data: pyc_modules },
        packages,
        known_modules,
    )?;
    meta_path_object.call_method(py, "clear", NoArgs, None)?;
    meta_path_object.call_method(py, "append", (unified_importer,), None)?;

    Ok(py.None())
}

/// Module initialization function.
///
/// This creates the Python module object.
///
/// We don't use the macros in the cpython crate because they are somewhat
/// opinionated about how things should work. e.g. they call
/// PyEval_InitThreads(), which is undesired. We want total control.
#[allow(non_snake_case)]
pub extern "C" fn PyInit__pyoxidizer_importer() -> *mut pyffi::PyObject {
    let py = unsafe { cpython::Python::assume_gil_acquired() };
    let module = unsafe { pyffi::PyModule_Create(&mut MODULE_DEF) };

    if module.is_null() {
        return module;
    }

    let module = match unsafe { PyObject::from_owned_ptr(py, module).cast_into::<PyModule>(py) } {
        Ok(m) => m,
        Err(e) => {
            PyErr::from(e).restore(py);
            return std::ptr::null_mut();
        }
    };

    match module_init(py, &module) {
        Ok(()) => module.into_object().steal_ptr(),
        Err(e) => {
            e.restore(py);
            std::ptr::null_mut()
        }
    }
}
