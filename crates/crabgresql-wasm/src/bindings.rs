//! The WIT glue: `crabgresql:db/database` in terms of [`Database`].
//!
//! Nothing but translation lives here — a resource handle, `RefCell` because
//! WIT resource methods take `&self` while a session is a stateful pipe, and
//! JSON in place of the richer types the component model could carry. Keeping
//! it this thin is what lets everything else be tested on the host.

use std::cell::RefCell;
use std::path::Path;

use crate::session::Database;

wit_bindgen::generate!({
    world: "database",
    path: "wit",
});

struct Component;

/// One open database. `RefCell` and not a lock: wasm is single-threaded, and a
/// reentrant call — `exec` from inside `exec` — cannot happen because the host
/// is blocked for the whole call.
struct Connection {
    db: RefCell<Database>,
}

impl exports::crabgresql::db::engine::GuestConnection for Connection {
    fn open(data_dir: String) -> Result<exports::crabgresql::db::engine::Connection, String> {
        let db = Database::open(Path::new(&data_dir)).map_err(|error| error.to_string())?;
        Ok(exports::crabgresql::db::engine::Connection::new(
            Connection {
                db: RefCell::new(db),
            },
        ))
    }

    fn exec(&self, sql: String) -> Result<String, String> {
        let mut db = self.db.borrow_mut();
        match db.exec(&sql) {
            Ok(Ok(output)) => Ok(output.to_json()),
            // A statement error: the SQLSTATE and message, as JSON.
            Ok(Err(error)) => Err(error),
            // The session itself is gone; the message is plain text because
            // there is no SQLSTATE for "the pipe closed".
            Err(error) => Err(error.to_string()),
        }
    }

    fn checkpoint(&self) -> Result<(), String> {
        self.db.borrow().checkpoint();
        Ok(())
    }
}

impl exports::crabgresql::db::engine::Guest for Component {
    type Connection = Connection;
}

export!(Component);
