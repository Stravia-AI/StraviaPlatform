wasmtime::component::bindgen!({
    world: "vendor-with-state",
    path: "../stravia-vendor-sdk/wit",
    with: {
        "wasi": wasmtime_wasi::p2::bindings,
        "stravia:net/types.options": crate::host::NetOptionsResource,
        "stravia:net/http.response": crate::host::HttpResponseResource,
        "stravia:net/websocket.connection": crate::host::WebSocketResource,
        "stravia:state/store.cell": crate::host::StateCellResource,
        "stravia:state/journal.entry": crate::host::JournalEntryResource,
    },
    imports: {
        "stravia:vendor/host@1.0.0": async | trappable,
        "stravia:net/types@1.1.0": async | trappable,
        "stravia:net/http@1.1.0": async | trappable,
        "stravia:net/websocket@1.1.0": async | trappable,
        "stravia:state/store@1.1.0": async | trappable,
        "stravia:state/volatile@1.1.0": async | trappable,
        "stravia:state/journal@1.1.0": async | trappable,
    },
    exports: {
        "execute": async | store,
        default: async,
    },
});

pub(crate) type VendorPre<T> = VendorWithStatePre<T>;
