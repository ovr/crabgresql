/**
 * The WASI 0.2 host the component runs against: an in-memory filesystem, plus
 * the handful of clock/random/stdio interfaces a database needs.
 *
 * Why not `@bytecodealliance/preview2-shim`? Its browser filesystem is a
 * demo-grade stand-in — `write` accepts only offset 0 and replaces the whole
 * file, `setSize`, `renameAt` and `unlinkFileAt` are `console.log` stubs — and
 * a storage engine is exactly the program those shortcuts break: the WAL writes
 * at offsets, truncates partial records away, and publishes the control file by
 * renaming it. Its Node filesystem refuses `mutate-directory`, which is what
 * wasi-libc asks for on any read-write open.
 *
 * So the host is ours. It is also the seam where a persistent backend (OPFS)
 * would go: everything below is a `Node` tree in RAM, and nothing above it
 * knows that.
 */

const symbolDispose = Symbol.dispose ?? Symbol.for('dispose');

// ---------------------------------------------------------------------------
// wasi:io
// ---------------------------------------------------------------------------

/** The `error` resource. Ours are always synchronous failures with a message. */
class IoError extends Error {
  toDebugString() {
    return this.message;
  }
}

/**
 * Every pollable here is already ready: the filesystem is synchronous and the
 * only timer is a deadline the guest asked to sleep until, which `block`
 * honors by spinning. Nothing else can be waited on, so nothing else can
 * block forever.
 */
class Pollable {
  #deadlineMs;

  constructor(deadlineMs = 0) {
    this.#deadlineMs = deadlineMs;
  }

  ready() {
    return Date.now() >= this.#deadlineMs;
  }

  block() {
    // A busy wait, on purpose: the guest is a synchronous call from JS, so
    // there is no event loop to yield to without returning first.
    while (!this.ready());
  }
}

class InputStream {
  #read;

  constructor(read) {
    this.#read = read;
  }

  read(len) {
    return this.#read(Number(len));
  }

  blockingRead(len) {
    return this.read(len);
  }

  subscribe() {
    return new Pollable();
  }

  [symbolDispose]() {}
}

class OutputStream {
  #write;
  #flush;

  constructor(write, flush = () => {}) {
    this.#write = write;
    this.#flush = flush;
  }

  /** No buffering, so there is always room; the number is what WASI calls a
   * "large enough" hint. */
  checkWrite() {
    return 1n << 20n;
  }

  write(contents) {
    this.#write(contents);
  }

  blockingWriteAndFlush(contents) {
    this.#write(contents);
    this.#flush();
  }

  flush() {
    this.#flush();
  }

  blockingFlush() {
    this.#flush();
  }

  subscribe() {
    return new Pollable();
  }

  [symbolDispose]() {}
}

export const error = { Error: IoError };

export const poll = {
  Pollable,
  poll(pollables) {
    const ready = [];
    for (let i = 0; i < pollables.length; i++) {
      if (pollables[i].ready()) ready.push(i);
    }
    if (ready.length === 0) {
      // Nothing is ready and nothing can become ready without the guest
      // returning first, so wait for the earliest deadline.
      pollables[0].block();
      return new Uint32Array([0]);
    }
    return new Uint32Array(ready);
  },
};

export const streams = { InputStream, OutputStream, Error: IoError };

// ---------------------------------------------------------------------------
// wasi:cli — stdio goes to the console, since the guest logs through it
// ---------------------------------------------------------------------------

const decoder = new TextDecoder();

function consoleStream(sink) {
  let pending = '';
  return new OutputStream(
    (contents) => {
      pending += decoder.decode(contents, { stream: true });
      const lines = pending.split('\n');
      pending = lines.pop() ?? '';
      for (const line of lines) sink(line);
    },
    () => {
      if (pending !== '') {
        sink(pending);
        pending = '';
      }
    },
  );
}

const stdoutStream = consoleStream((line) => console.log(line));
const stderrStream = consoleStream((line) => console.error(line));
const stdinStream = new InputStream(() => new Uint8Array(0));

export const stdout = { getStdout: () => stdoutStream };
export const stderr = { getStderr: () => stderrStream };
export const stdin = { getStdin: () => stdinStream };

class TerminalInput {}
class TerminalOutput {}

export const terminalInput = { TerminalInput };
export const terminalOutput = { TerminalOutput };
export const terminalStdin = { getTerminalStdin: () => undefined };
export const terminalStdout = { getTerminalStdout: () => undefined };
export const terminalStderr = { getTerminalStderr: () => undefined };

/**
 * The engine reads its tuning knobs (`PGDATA`, buffer sizes, `RUST_LOG`) from
 * the environment; `setEnvironment` is how an embedder sets them before the
 * first call.
 */
let environmentPairs = [];

export function setEnvironment(vars) {
  environmentPairs = Object.entries(vars);
}

export const environment = {
  getEnvironment: () => environmentPairs,
  getArguments: () => [],
  initialCwd: () => '/',
};

export const exit = {
  exit(status) {
    throw new Error(`the component called exit(${status.tag})`);
  },
};

// ---------------------------------------------------------------------------
// wasi:clocks, wasi:random
// ---------------------------------------------------------------------------

export const monotonicClock = {
  Pollable,
  now: () => BigInt(Math.round(performance.now() * 1e6)),
  resolution: () => 1000n,
  subscribeInstant: (when) =>
    new Pollable(Date.now() + Number(when - monotonicClock.now()) / 1e6),
  subscribeDuration: (duration) => new Pollable(Date.now() + Number(duration) / 1e6),
};

export const wallClock = {
  now() {
    const ms = Date.now();
    return {
      seconds: BigInt(Math.floor(ms / 1000)),
      nanoseconds: (ms % 1000) * 1e6,
    };
  },
  resolution: () => ({ seconds: 0n, nanoseconds: 1e6 }),
};

function randomBytes(length) {
  const bytes = new Uint8Array(length);
  crypto.getRandomValues(bytes);
  return bytes;
}

function randomU64() {
  return new BigUint64Array(randomBytes(8).buffer)[0];
}

export const random = {
  getRandomBytes: (len) => randomBytes(Number(len)),
  getRandomU64: randomU64,
};

export const insecureSeed = { insecureSeed: () => [randomU64(), randomU64()] };
export const insecure = {
  getInsecureRandomBytes: (len) => randomBytes(Number(len)),
  getInsecureRandomU64: randomU64,
};

// ---------------------------------------------------------------------------
// wasi:filesystem — an in-memory tree
// ---------------------------------------------------------------------------

/** A directory: named children. */
function newDirectory() {
  return { type: 'directory', entries: new Map() };
}

/**
 * A file: a capacity-doubling buffer plus the size actually written, so an
 * append does not copy the whole file each time.
 */
function newFile() {
  return { type: 'regular-file', data: new Uint8Array(0), size: 0 };
}

function fileBytes(file) {
  return file.data.subarray(0, file.size);
}

function reserve(file, needed) {
  if (needed <= file.data.byteLength) return;
  let capacity = Math.max(file.data.byteLength * 2, 1024);
  while (capacity < needed) capacity *= 2;
  const grown = new Uint8Array(capacity);
  grown.set(fileBytes(file));
  file.data = grown;
}

function writeAt(file, offset, bytes) {
  const end = offset + bytes.byteLength;
  reserve(file, end);
  // A write past the end leaves a hole, which POSIX says reads as zeroes; the
  // buffer is zero-filled already, but the bytes between the old size and the
  // offset have to be *counted* as file content.
  if (offset > file.size) file.data.fill(0, file.size, offset);
  file.data.set(bytes, offset);
  if (end > file.size) file.size = end;
}

function splitPath(path) {
  return path.split('/').filter((part) => part !== '' && part !== '.');
}

/** Walk to the directory holding `path`'s last component. */
function walkToParent(root, path) {
  const parts = splitPath(path);
  if (parts.length === 0) throw 'invalid';
  let node = root;
  for (const part of parts.slice(0, -1)) {
    const child = node.entries?.get(part);
    if (child === undefined) throw 'no-entry';
    if (child.type !== 'directory') throw 'not-directory';
    node = child;
  }
  if (node.type !== 'directory') throw 'not-directory';
  return [node, parts[parts.length - 1]];
}

function lookup(root, path) {
  const parts = splitPath(path);
  let node = root;
  for (const part of parts) {
    if (node.type !== 'directory') throw 'not-directory';
    const child = node.entries.get(part);
    if (child === undefined) throw 'no-entry';
    node = child;
  }
  return node;
}

const emptyDatetime = { seconds: 0n, nanoseconds: 0 };

function statOf(node) {
  return {
    type: node.type,
    linkCount: 1n,
    size: node.type === 'regular-file' ? BigInt(node.size) : 0n,
    dataAccessTimestamp: emptyDatetime,
    dataModificationTimestamp: emptyDatetime,
    statusChangeTimestamp: emptyDatetime,
  };
}

class DirectoryEntryStream {
  #entries;
  #next = 0;

  constructor(entries) {
    this.#entries = entries;
  }

  readDirectoryEntry() {
    if (this.#next >= this.#entries.length) return undefined;
    const [name, node] = this.#entries[this.#next++];
    return { type: node.type, name };
  }

  [symbolDispose]() {}
}

class Descriptor {
  #node;

  constructor(node) {
    this.#node = node;
  }

  /** For `renameAt`, which needs the *other* descriptor's node. */
  static nodeOf(descriptor) {
    return descriptor.#node;
  }

  #directory() {
    if (this.#node.type !== 'directory') throw 'not-directory';
    return this.#node;
  }

  #file() {
    if (this.#node.type !== 'regular-file') throw 'is-directory';
    return this.#node;
  }

  readViaStream(offset) {
    const file = this.#file();
    let at = Number(offset);
    return new InputStream((len) => {
      const bytes = fileBytes(file);
      if (at >= bytes.byteLength) throw { tag: 'closed' };
      const chunk = bytes.slice(at, at + len);
      at += chunk.byteLength;
      return chunk;
    });
  }

  writeViaStream(offset) {
    const file = this.#file();
    let at = Number(offset);
    return new OutputStream((contents) => {
      writeAt(file, at, contents);
      at += contents.byteLength;
    });
  }

  appendViaStream() {
    const file = this.#file();
    return new OutputStream((contents) => writeAt(file, file.size, contents));
  }

  advise() {}

  syncData() {}

  sync() {}

  getFlags() {
    return { read: true, write: true };
  }

  getType() {
    return this.#node.type;
  }

  setSize(size) {
    const file = this.#file();
    const wanted = Number(size);
    if (wanted > file.size) {
      reserve(file, wanted);
      file.data.fill(0, file.size, wanted);
    }
    file.size = wanted;
  }

  setTimes() {}

  setTimesAt() {}

  readDirectory() {
    const entries = [...this.#directory().entries.entries()].sort(([a], [b]) =>
      a < b ? -1 : a > b ? 1 : 0,
    );
    return new DirectoryEntryStream(entries);
  }

  createDirectoryAt(path) {
    const [parent, name] = walkToParent(this.#directory(), path);
    if (parent.entries.has(name)) throw 'exist';
    parent.entries.set(name, newDirectory());
  }

  stat() {
    return statOf(this.#node);
  }

  statAt(_pathFlags, path) {
    return statOf(lookup(this.#directory(), path));
  }

  openAt(_pathFlags, path, openFlags = {}, _flags = {}) {
    const directory = this.#directory();
    if (splitPath(path).length === 0) return new Descriptor(directory);
    const [parent, name] = walkToParent(directory, path);
    let node = parent.entries.get(name);
    if (node === undefined) {
      if (!openFlags.create) throw 'no-entry';
      node = openFlags.directory ? newDirectory() : newFile();
      parent.entries.set(name, node);
    } else if (openFlags.exclusive) {
      throw 'exist';
    }
    if (openFlags.directory && node.type !== 'directory') throw 'not-directory';
    if (openFlags.truncate && node.type === 'regular-file') node.size = 0;
    return new Descriptor(node);
  }

  readlinkAt() {
    // Nothing here is ever a symlink, so a caller that got this far was
    // told the entry is one — which cannot happen.
    throw 'invalid';
  }

  removeDirectoryAt(path) {
    const [parent, name] = walkToParent(this.#directory(), path);
    const node = parent.entries.get(name);
    if (node === undefined) throw 'no-entry';
    if (node.type !== 'directory') throw 'not-directory';
    if (node.entries.size > 0) throw 'not-empty';
    parent.entries.delete(name);
  }

  /**
   * Rename, replacing the destination if it exists — which is the property the
   * WAL relies on: the control file is published by writing a temporary file
   * and renaming it over the live one, and a rename that refused to overwrite
   * would leave the cluster with no control file at all.
   */
  renameAt(oldPath, newDescriptor, newPath) {
    const [fromParent, fromName] = walkToParent(this.#directory(), oldPath);
    const node = fromParent.entries.get(fromName);
    if (node === undefined) throw 'no-entry';
    const [toParent, toName] = walkToParent(
      Descriptor.nodeOf(newDescriptor),
      newPath,
    );
    fromParent.entries.delete(fromName);
    toParent.entries.set(toName, node);
  }

  unlinkFileAt(path) {
    const [parent, name] = walkToParent(this.#directory(), path);
    const node = parent.entries.get(name);
    if (node === undefined) throw 'no-entry';
    if (node.type === 'directory') throw 'is-directory';
    parent.entries.delete(name);
  }

  linkAt() {
    throw 'unsupported';
  }

  symlinkAt() {
    throw 'unsupported';
  }

  isSameObject(other) {
    return this.#node === Descriptor.nodeOf(other);
  }

  metadataHash() {
    return this.#hash(this.#node);
  }

  metadataHashAt(_pathFlags, path) {
    return this.#hash(lookup(this.#directory(), path));
  }

  /**
   * An identity hash, which is what the guest uses it for (`same file?`), built
   * from a per-node serial rather than from contents — two files with the same
   * bytes are still two files.
   */
  #hash(node) {
    if (node.serial === undefined) node.serial = ++Descriptor.serials;
    return { lower: BigInt(node.serial), upper: 0n };
  }

  static serials = 0;

  [symbolDispose]() {}
}

/** The filesystem every `Descriptor` in this module is a view into. */
let root = newDirectory();

/** Throw away the contents and start over — one database per page, per reset. */
export function resetFilesystem() {
  root = newDirectory();
}

export const types = {
  Descriptor,
  DirectoryEntryStream,
  filesystemErrorCode: () => undefined,
};

export const preopens = {
  getDirectories: () => [[new Descriptor(root), '/']],
};
