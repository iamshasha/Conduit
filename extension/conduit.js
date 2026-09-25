// Conduit — TurboWarp extension.
//
// Talks to the local Conduit host app over a WebSocket on 127.0.0.1.
// Load it unsandboxed: a sandboxed extension runs in a null-origin iframe and
// the host app rejects null origins on purpose.
//
//   http://127.0.0.1:8765/turbowarp/extension.js
//
(function (Scratch) {
  'use strict';

  if (!Scratch.extensions.unsandboxed) {
    throw new Error('The Conduit extension must be loaded unsandboxed.');
  }

  const DEFAULT_PORT = 8765;
  const PERMS = ['fs', 'hw', 'launch', 'system', 'process', 'power', 'clipboard', 'notify', 'folder'];

  class Conduit {
    constructor() {
      this.port = DEFAULT_PORT;
      this.ws = null;
      this.nextId = 1;
      this.pending = new Map();
      this.lastError = '';
      this.grantedPerms = [];
      this.connecting = null;
      // File-change watching: the host pushes fs.change events with no id.
      this.watchId = null;
      this._pendingChange = false;
      this._lastChanges = [];
    }

    base() {
      return `http://127.0.0.1:${this.port}`;
    }

    tokenKey() {
      return `conduit.token.${this.port}`;
    }

    getToken() {
      try {
        return localStorage.getItem(this.tokenKey()) || '';
      } catch (e) {
        return this.memToken || '';
      }
    }

    setToken(t) {
      this.memToken = t;
      try {
        localStorage.setItem(this.tokenKey(), t);
      } catch (e) {
        /* private mode: keep it in memory only */
      }
    }

    isConnected() {
      return !!this.ws && this.ws.readyState === 1 && this.authed;
    }

    // --- transport ---------------------------------------------------------

    async pair() {
      const r = await fetch(`${this.base()}/pair`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ perms: PERMS }),
      });
      const j = await r.json();
      if (!j.ok) throw new Error(j.error ? j.error.message : `pair failed (${r.status})`);
      this.setToken(j.result.token);
      return j.result.token;
    }

    /** Idempotent connect; rejects on failure. */
    ensure(port) {
      if (port) this.port = port;
      if (this.isConnected()) return Promise.resolve();
      if (!this.connecting) {
        this.connecting = this._connect().finally(() => {
          this.connecting = null;
        });
      }
      return this.connecting;
    }

    async _connect() {
      let token = this.getToken();
      if (!token) token = await this.pair();
      try {
        await this._openSocket(token);
      } catch (e) {
        // A stale token survives a host restart with a fresh data dir; re-pair
        // once before giving up.
        if (String(e.message).indexOf('unauthorized') === -1) throw e;
        token = await this.pair();
        await this._openSocket(token);
      }
    }

    _openSocket(token) {
      return new Promise((resolve, reject) => {
        let ws;
        try {
          ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws`);
        } catch (e) {
          reject(new Error(`cannot open socket: ${e.message}`));
          return;
        }
        this.authed = false;
        const fail = (msg) => {
          this.ws = null;
          reject(new Error(msg));
        };
        ws.onmessage = (ev) => this._onMessage(ev);
        ws.onerror = () => fail('Conduit host not reachable — is the app running?');
        ws.onclose = () => {
          this.ws = null;
          this.authed = false;
          for (const [, p] of this.pending) p.reject(new Error('socket closed'));
          this.pending.clear();
        };
        ws.onopen = () => {
          this.ws = ws;
          this._send('auth', { token })
            .then((res) => {
              this.authed = true;
              this.grantedPerms = res.perms || [];
              resolve();
            })
            .catch((e) => fail(e.message));
        };
      });
    }

    _onMessage(ev) {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch (e) {
        return;
      }
      // Pushed events carry `event` and no id.
      if (msg.event) {
        if (msg.event === 'fs.change') {
          this._pendingChange = true;
          this._lastChanges = msg.changes || [];
        } else if (msg.event === 'watch.stopped') {
          this.watchId = null;
        }
        return;
      }
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.result);
      else p.reject(new Error(msg.error ? `${msg.error.code}: ${msg.error.message}` : 'unknown error'));
    }

    _send(method, params) {
      return new Promise((resolve, reject) => {
        if (!this.ws || this.ws.readyState !== 1) {
          reject(new Error('not connected'));
          return;
        }
        const id = this.nextId++;
        this.pending.set(id, { resolve, reject });
        this.ws.send(JSON.stringify({ id, method, params: params || {} }));
        setTimeout(() => {
          if (this.pending.has(id)) {
            this.pending.delete(id);
            reject(new Error(`timeout on ${method}`));
          }
        }, 120000);
      });
    }

    /** Auto-connects, records failures in `lastError`. */
    async call(method, params) {
      try {
        await this.ensure();
        const r = await this._send(method, params);
        this.lastError = '';
        return r;
      } catch (e) {
        this.lastError = e.message || String(e);
        throw e;
      }
    }

    async soft(method, params, fallback) {
      try {
        return await this.call(method, params);
      } catch (e) {
        return fallback;
      }
    }

    // --- blocks ------------------------------------------------------------

    getInfo() {
      return {
        id: 'conduit',
        name: 'Conduit',
        color1: '#2f6f4f',
        color2: '#245740',
        blocks: [
          { opcode: 'connect', blockType: Scratch.BlockType.COMMAND,
            text: 'connect to Conduit on port [PORT]',
            arguments: { PORT: { type: Scratch.ArgumentType.NUMBER, defaultValue: DEFAULT_PORT } } },
          { opcode: 'connected', blockType: Scratch.BlockType.BOOLEAN, text: 'connected?' },
          { opcode: 'error', blockType: Scratch.BlockType.REPORTER, text: 'last Conduit error' },
          { opcode: 'perms', blockType: Scratch.BlockType.REPORTER, text: 'granted permissions' },
          '---',
          { opcode: 'writeFile', blockType: Scratch.BlockType.COMMAND,
            text: 'save [DATA] to file [PATH]',
            arguments: {
              DATA: { type: Scratch.ArgumentType.STRING, defaultValue: 'hello' },
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' },
            } },
          { opcode: 'appendFile', blockType: Scratch.BlockType.COMMAND,
            text: 'append [DATA] to file [PATH]',
            arguments: {
              DATA: { type: Scratch.ArgumentType.STRING, defaultValue: 'line\n' },
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'log.txt' },
            } },
          { opcode: 'readFile', blockType: Scratch.BlockType.REPORTER,
            text: 'contents of file [PATH]',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' } } },
          { opcode: 'fileExists', blockType: Scratch.BlockType.BOOLEAN,
            text: 'file [PATH] exists?',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' } } },
          { opcode: 'fileSize', blockType: Scratch.BlockType.REPORTER,
            text: 'size of file [PATH]',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' } } },
          { opcode: 'listFiles', blockType: Scratch.BlockType.REPORTER,
            text: 'files in folder [PATH]',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: '' } } },
          { opcode: 'deleteFile', blockType: Scratch.BlockType.COMMAND,
            text: 'delete [PATH]',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' } } },
          { opcode: 'makeFolder', blockType: Scratch.BlockType.COMMAND,
            text: 'make folder [PATH]',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves' } } },
          { opcode: 'copyFile', blockType: Scratch.BlockType.COMMAND,
            text: 'copy file [FROM] to [TO]',
            arguments: {
              FROM: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/slot1.txt' },
              TO: { type: Scratch.ArgumentType.STRING, defaultValue: 'saves/backup.txt' },
            } },
          { opcode: 'moveFile', blockType: Scratch.BlockType.COMMAND,
            text: 'move file [FROM] to [TO]',
            arguments: {
              FROM: { type: Scratch.ArgumentType.STRING, defaultValue: 'a.txt' },
              TO: { type: Scratch.ArgumentType.STRING, defaultValue: 'b.txt' },
            } },
          { opcode: 'revealFile', blockType: Scratch.BlockType.COMMAND,
            text: 'show [PATH] in file explorer',
            arguments: { PATH: { type: Scratch.ArgumentType.STRING, defaultValue: '' } } },
          { opcode: 'quota', blockType: Scratch.BlockType.REPORTER,
            text: '[FIELD] of storage quota',
            arguments: { FIELD: { type: Scratch.ArgumentType.STRING, menu: 'quotaMenu' } } },
          '---',
          // Easy key/value store: no paths, no JSON. Everything lives in one
          // file per store so a project can "save the game" in two blocks.
          { opcode: 'kvSet', blockType: Scratch.BlockType.COMMAND,
            text: 'save [VALUE] as [KEY]',
            arguments: {
              KEY: { type: Scratch.ArgumentType.STRING, defaultValue: 'score' },
              VALUE: { type: Scratch.ArgumentType.STRING, defaultValue: '100' },
            } },
          { opcode: 'kvGet', blockType: Scratch.BlockType.REPORTER,
            text: 'load [KEY]',
            arguments: { KEY: { type: Scratch.ArgumentType.STRING, defaultValue: 'score' } } },
          { opcode: 'kvHas', blockType: Scratch.BlockType.BOOLEAN,
            text: 'has [KEY]?',
            arguments: { KEY: { type: Scratch.ArgumentType.STRING, defaultValue: 'score' } } },
          { opcode: 'kvDelete', blockType: Scratch.BlockType.COMMAND,
            text: 'delete saved [KEY]',
            arguments: { KEY: { type: Scratch.ArgumentType.STRING, defaultValue: 'score' } } },
          { opcode: 'kvKeys', blockType: Scratch.BlockType.REPORTER, text: 'all saved keys' },
          { opcode: 'kvStore', blockType: Scratch.BlockType.COMMAND,
            text: 'use save file [NAME]',
            arguments: { NAME: { type: Scratch.ArgumentType.STRING, defaultValue: 'default' } } },
          '---',
          { opcode: 'hwField', blockType: Scratch.BlockType.REPORTER,
            text: 'hardware [FIELD]',
            arguments: { FIELD: { type: Scratch.ArgumentType.STRING, menu: 'hwMenu' } } },
          { opcode: 'hwInfo', blockType: Scratch.BlockType.REPORTER, text: 'hardware info as JSON' },
          { opcode: 'liveStat', blockType: Scratch.BlockType.REPORTER,
            text: 'live [FIELD]',
            arguments: { FIELD: { type: Scratch.ArgumentType.STRING, menu: 'statMenu' } } },
          { opcode: 'onBattery', blockType: Scratch.BlockType.BOOLEAN, text: 'on battery power?' },
          '---',
          { opcode: 'launch', blockType: Scratch.BlockType.COMMAND,
            text: 'launch app [PATH] with args [ARGS]',
            arguments: {
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'C:\\Windows\\System32\\notepad.exe' },
              ARGS: { type: Scratch.ArgumentType.STRING, defaultValue: '' },
            } },
          { opcode: 'mediaKey', blockType: Scratch.BlockType.COMMAND,
            text: 'press media key [KEY]',
            arguments: { KEY: { type: Scratch.ArgumentType.STRING, menu: 'mediaMenu' } } },
          { opcode: 'setVolume', blockType: Scratch.BlockType.COMMAND,
            text: 'set system volume to [LEVEL] %',
            arguments: { LEVEL: { type: Scratch.ArgumentType.NUMBER, defaultValue: 50 } } },
          { opcode: 'changeVolume', blockType: Scratch.BlockType.COMMAND,
            text: 'change system volume by [DELTA]',
            arguments: { DELTA: { type: Scratch.ArgumentType.NUMBER, defaultValue: 10 } } },
          { opcode: 'volume', blockType: Scratch.BlockType.REPORTER, text: 'system volume' },
          { opcode: 'setMute', blockType: Scratch.BlockType.COMMAND,
            text: '[STATE] system sound',
            arguments: { STATE: { type: Scratch.ArgumentType.STRING, menu: 'muteMenu' } } },
          { opcode: 'muted', blockType: Scratch.BlockType.BOOLEAN, text: 'system sound muted?' },
          { opcode: 'mediaControl', blockType: Scratch.BlockType.COMMAND,
            text: 'media: [ACTION]',
            arguments: { ACTION: { type: Scratch.ArgumentType.STRING, menu: 'mediaActionMenu' } } },
          { opcode: 'nowPlaying', blockType: Scratch.BlockType.REPORTER,
            text: 'now playing [FIELD]',
            arguments: { FIELD: { type: Scratch.ArgumentType.STRING, menu: 'nowPlayingMenu' } } },
          { opcode: 'openUrl', blockType: Scratch.BlockType.COMMAND,
            text: 'open [URL] in browser',
            arguments: { URL: { type: Scratch.ArgumentType.STRING, defaultValue: 'https://scratch.mit.edu' } } },
          { opcode: 'notify', blockType: Scratch.BlockType.COMMAND,
            text: 'notify [TITLE] : [BODY]',
            arguments: {
              TITLE: { type: Scratch.ArgumentType.STRING, defaultValue: 'My game' },
              BODY: { type: Scratch.ArgumentType.STRING, defaultValue: 'Level complete!' },
            } },
          { opcode: 'clipWrite', blockType: Scratch.BlockType.COMMAND,
            text: 'copy [TEXT] to clipboard',
            arguments: { TEXT: { type: Scratch.ArgumentType.STRING, defaultValue: 'hello' } } },
          { opcode: 'clipRead', blockType: Scratch.BlockType.REPORTER, text: 'clipboard text (asks permission)' },
          '---',
          { opcode: 'processes', blockType: Scratch.BlockType.REPORTER, text: 'running processes as JSON' },
          { opcode: 'killPid', blockType: Scratch.BlockType.COMMAND,
            text: 'end process id [PID]',
            arguments: { PID: { type: Scratch.ArgumentType.NUMBER, defaultValue: 0 } } },
          { opcode: 'power', blockType: Scratch.BlockType.COMMAND,
            text: '[ACTION] the computer',
            arguments: { ACTION: { type: Scratch.ArgumentType.STRING, menu: 'powerMenu' } } },
          { opcode: 'isAdmin', blockType: Scratch.BlockType.BOOLEAN, text: 'running as administrator?' },
          { opcode: 'requestAdmin', blockType: Scratch.BlockType.COMMAND, text: 'request administrator rights' },
          '---',
          // Host folders the user picks once, then the project reads/writes inside.
          { opcode: 'pickFolder', blockType: Scratch.BlockType.REPORTER,
            text: 'ask for a folder named [NAME] (returns its id)',
            arguments: { NAME: { type: Scratch.ArgumentType.STRING, defaultValue: 'my project' } } },
          { opcode: 'grantedFolders', blockType: Scratch.BlockType.REPORTER, text: 'granted folders as JSON' },
          { opcode: 'folderWrite', blockType: Scratch.BlockType.COMMAND,
            text: 'write [DATA] to [PATH] in folder [ID]',
            arguments: {
              DATA: { type: Scratch.ArgumentType.STRING, defaultValue: 'hello' },
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'notes.txt' },
              ID: { type: Scratch.ArgumentType.STRING, defaultValue: '' },
            } },
          { opcode: 'folderRead', blockType: Scratch.BlockType.REPORTER,
            text: 'read [PATH] in folder [ID]',
            arguments: {
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: 'notes.txt' },
              ID: { type: Scratch.ArgumentType.STRING, defaultValue: '' },
            } },
          { opcode: 'folderList', blockType: Scratch.BlockType.REPORTER,
            text: 'list [PATH] in folder [ID]',
            arguments: {
              PATH: { type: Scratch.ArgumentType.STRING, defaultValue: '' },
              ID: { type: Scratch.ArgumentType.STRING, defaultValue: '' },
            } },
          { opcode: 'forgetFolder', blockType: Scratch.BlockType.COMMAND,
            text: 'forget folder [ID]',
            arguments: { ID: { type: Scratch.ArgumentType.STRING, defaultValue: '' } } },
          '---',
          // Live file-change watching.
          { opcode: 'watchSandbox', blockType: Scratch.BlockType.COMMAND, text: 'watch my files for changes' },
          { opcode: 'watchFolder', blockType: Scratch.BlockType.COMMAND,
            text: 'watch folder [ID] for changes',
            arguments: { ID: { type: Scratch.ArgumentType.STRING, defaultValue: '' } } },
          { opcode: 'stopWatching', blockType: Scratch.BlockType.COMMAND, text: 'stop watching for changes' },
          { opcode: 'whenFilesChange', blockType: Scratch.BlockType.HAT, text: 'when files change' },
          { opcode: 'changedFiles', blockType: Scratch.BlockType.REPORTER, text: 'changed files as JSON' },
          '---',
          { opcode: 'raw', blockType: Scratch.BlockType.REPORTER,
            text: 'call [METHOD] with params [PARAMS]',
            arguments: {
              METHOD: { type: Scratch.ArgumentType.STRING, defaultValue: 'ping' },
              PARAMS: { type: Scratch.ArgumentType.STRING, defaultValue: '{}' },
            } },
        ],
        menus: {
          hwMenu: {
            acceptReporters: true,
            items: ['os', 'os version', 'arch', 'cpu', 'logical cores', 'physical cores',
                    'cpu mhz', 'ram total mb', 'ram free mb', 'disk total gb', 'disk free gb'],
          },
          quotaMenu: { acceptReporters: true, items: ['used bytes', 'limit bytes', 'files', 'max file bytes'] },
          statMenu: { acceptReporters: true,
            items: ['cpu %', 'ram used mb', 'ram %', 'net down kb/s', 'net up kb/s', 'battery %', 'uptime seconds'] },
          mediaMenu: { acceptReporters: true,
            items: ['play_pause', 'next', 'prev', 'stop', 'volume_up', 'volume_down', 'mute'] },
          powerMenu: { acceptReporters: true,
            items: ['lock', 'sleep', 'logoff', 'restart', 'shutdown', 'abort'] },
          muteMenu: { acceptReporters: true, items: ['mute', 'unmute'] },
          mediaActionMenu: { acceptReporters: true, items: ['toggle', 'play', 'pause', 'next', 'prev', 'stop'] },
          nowPlayingMenu: { acceptReporters: true, items: ['title', 'artist', 'album', 'status', 'app'] },
        },
      };
    }

    // The current key/value store file, swappable with `use save file`.
    kvFile() {
      return 'kv/' + (this.storeName || 'default') + '.json';
    }
    async kvLoad() {
      const r = await this.soft('fs.read', { path: this.kvFile() }, null);
      if (!r || !r.data) return {};
      try { return JSON.parse(r.data) || {}; } catch (e) { return {}; }
    }
    kvSave(obj) {
      return this.call('fs.write', { path: this.kvFile(), data: JSON.stringify(obj) });
    }

    connect(args) {
      return this.ensure(Number(args.PORT) || DEFAULT_PORT).then(
        () => {
          this.lastError = '';
        },
        (e) => {
          this.lastError = e.message || String(e);
        }
      );
    }

    connected() {
      return this.isConnected();
    }

    error() {
      return this.lastError;
    }

    async perms() {
      const r = await this.soft('perms', {}, { perms: [] });
      return (r.perms || []).join(', ');
    }

    writeFile(args) {
      return this.call('fs.write', { path: Scratch.Cast.toString(args.PATH), data: Scratch.Cast.toString(args.DATA) })
        .then(() => undefined, () => undefined);
    }

    appendFile(args) {
      return this.call('fs.write', {
        path: Scratch.Cast.toString(args.PATH),
        data: Scratch.Cast.toString(args.DATA),
        append: true,
      }).then(() => undefined, () => undefined);
    }

    async readFile(args) {
      const r = await this.soft('fs.read', { path: Scratch.Cast.toString(args.PATH) }, null);
      return r ? r.data : '';
    }

    async fileExists(args) {
      const r = await this.soft('fs.stat', { path: Scratch.Cast.toString(args.PATH) }, { exists: false });
      return !!r.exists;
    }

    async fileSize(args) {
      const r = await this.soft('fs.stat', { path: Scratch.Cast.toString(args.PATH) }, { size: -1 });
      return r.exists ? r.size : -1;
    }

    async listFiles(args) {
      const r = await this.soft('fs.list', { path: Scratch.Cast.toString(args.PATH) }, { entries: [] });
      return JSON.stringify((r.entries || []).map((e) => (e.dir ? `${e.name}/` : e.name)));
    }

    deleteFile(args) {
      return this.call('fs.delete', { path: Scratch.Cast.toString(args.PATH), recursive: true })
        .then(() => undefined, () => undefined);
    }

    makeFolder(args) {
      return this.call('fs.mkdir', { path: Scratch.Cast.toString(args.PATH) })
        .then(() => undefined, () => undefined);
    }

    copyFile(args) {
      return this.call('fs.copy', { from: Scratch.Cast.toString(args.FROM), to: Scratch.Cast.toString(args.TO), overwrite: true })
        .then(() => undefined, () => undefined);
    }
    moveFile(args) {
      return this.call('fs.move', { from: Scratch.Cast.toString(args.FROM), to: Scratch.Cast.toString(args.TO), overwrite: true })
        .then(() => undefined, () => undefined);
    }
    // --- granted host folders ---
    async pickFolder(args) {
      const r = await this.soft('folder.pick', { name: Scratch.Cast.toString(args.NAME) }, null);
      return r ? r.id : '';
    }
    async grantedFolders() {
      const r = await this.soft('folder.granted', {}, { folders: [] });
      return JSON.stringify(r.folders || []);
    }
    folderWrite(args) {
      return this.call('folder.write', {
        id: Scratch.Cast.toString(args.ID),
        path: Scratch.Cast.toString(args.PATH),
        data: Scratch.Cast.toString(args.DATA),
      }).then(() => undefined, () => undefined);
    }
    async folderRead(args) {
      const r = await this.soft('folder.read', { id: Scratch.Cast.toString(args.ID), path: Scratch.Cast.toString(args.PATH) }, null);
      return r ? r.data : '';
    }
    async folderList(args) {
      const r = await this.soft('folder.list', { id: Scratch.Cast.toString(args.ID), path: Scratch.Cast.toString(args.PATH) }, { entries: [] });
      return JSON.stringify((r.entries || []).map((e) => (e.dir ? `${e.name}/` : e.name)));
    }
    forgetFolder(args) {
      return this.call('folder.forget', { id: Scratch.Cast.toString(args.ID) }).then(() => undefined, () => undefined);
    }

    // --- live file-change watching ---
    async watchSandbox() {
      const r = await this.soft('watch', { scope: 'sandbox' }, null);
      if (r) this.watchId = r.watch;
    }
    async watchFolder(args) {
      const r = await this.soft('watch', { scope: 'folder', id: Scratch.Cast.toString(args.ID) }, null);
      if (r) this.watchId = r.watch;
    }
    stopWatching() {
      if (this.watchId == null) return;
      const id = this.watchId;
      this.watchId = null;
      return this.call('unwatch', { watch: id }).then(() => undefined, () => undefined);
    }
    whenFilesChange() {
      // Edge-triggered: report the pending change once, then clear it so the
      // next event fires the hat again.
      if (this._pendingChange) {
        this._pendingChange = false;
        return true;
      }
      return false;
    }
    changedFiles() {
      return JSON.stringify(this._lastChanges || []);
    }

    revealFile(args) {
      return this.call('fs.reveal', { path: Scratch.Cast.toString(args.PATH) }).then(() => undefined, () => undefined);
    }

    // ---- easy key/value store ----
    kvStore(args) { this.storeName = Scratch.Cast.toString(args.NAME).replace(/[^\w.-]/g, '_') || 'default'; }
    async kvSet(args) {
      const obj = await this.kvLoad();
      obj[Scratch.Cast.toString(args.KEY)] = Scratch.Cast.toString(args.VALUE);
      await this.kvSave(obj).catch(() => {});
    }
    async kvGet(args) {
      const obj = await this.kvLoad();
      const v = obj[Scratch.Cast.toString(args.KEY)];
      return v == null ? '' : v;
    }
    async kvHas(args) {
      const obj = await this.kvLoad();
      return Object.prototype.hasOwnProperty.call(obj, Scratch.Cast.toString(args.KEY));
    }
    async kvDelete(args) {
      const obj = await this.kvLoad();
      delete obj[Scratch.Cast.toString(args.KEY)];
      await this.kvSave(obj).catch(() => {});
    }
    async kvKeys() {
      return JSON.stringify(Object.keys(await this.kvLoad()));
    }

    async quota(args) {
      const r = await this.soft('fs.quota', {}, null);
      if (!r) return -1;
      switch (Scratch.Cast.toString(args.FIELD)) {
        case 'used bytes': return r.used;
        case 'limit bytes': return r.limit;
        case 'files': return r.files;
        case 'max file bytes': return r.max_file_size;
        default: return -1;
      }
    }

    async hwInfo() {
      const r = await this.soft('hw.info', {}, null);
      return r ? JSON.stringify(r) : '';
    }

    async hwField(args) {
      const r = await this.soft('hw.info', {}, null);
      if (!r) return '';
      const mb = (b) => Math.round(b / (1024 * 1024));
      const gb = (b) => Math.round((b / (1024 * 1024 * 1024)) * 10) / 10;
      const disk = (r.disks && r.disks[0]) || { total: 0, available: 0 };
      switch (Scratch.Cast.toString(args.FIELD)) {
        case 'os': return r.os.name || '';
        case 'os version': return r.os.version || '';
        case 'arch': return r.os.arch || '';
        case 'cpu': return r.cpu.brand || '';
        case 'logical cores': return r.cpu.logical_cores || 0;
        case 'physical cores': return r.cpu.physical_cores || 0;
        case 'cpu mhz': return r.cpu.mhz || 0;
        case 'ram total mb': return mb(r.memory.total);
        case 'ram free mb': return mb(r.memory.available);
        case 'disk total gb': return gb(disk.total);
        case 'disk free gb': return gb(disk.available);
        default: return '';
      }
    }

    launch(args) {
      const argv = Scratch.Cast.toString(args.ARGS).trim();
      return this.call('app.launch', {
        path: Scratch.Cast.toString(args.PATH),
        // ponytail: whitespace split. Quote-aware parsing only if someone needs
        // an argument with a space in it — use the raw call block meanwhile.
        args: argv ? argv.split(/\s+/) : [],
      }).then(() => undefined, () => undefined);
    }

    async liveStat(args) {
      const s = await this.soft('sys.stats', {}, null);
      if (!s) return -1;
      const mb = (b) => Math.round(b / (1024 * 1024));
      switch (Scratch.Cast.toString(args.FIELD)) {
        case 'cpu %': return Math.round(s.cpu);
        case 'ram used mb': return mb(s.mem_used);
        case 'ram %': return Math.round(100 * s.mem_used / s.mem_total);
        case 'net down kb/s': return Math.round(s.net_rx / 1024);
        case 'net up kb/s': return Math.round(s.net_tx / 1024);
        case 'battery %': return s.battery && s.battery.present ? (s.battery.percent != null ? s.battery.percent : -1) : -1;
        case 'uptime seconds': return s.uptime;
        default: return -1;
      }
    }
    async onBattery() {
      const b = await this.soft('sys.battery', {}, null);
      return !!(b && b.present && !b.on_ac);
    }

    mediaKey(args) {
      return this.call('sys.media', { key: Scratch.Cast.toString(args.KEY) }).then(() => undefined, () => undefined);
    }
    setVolume(args) {
      const level = Math.max(0, Math.min(100, Scratch.Cast.toNumber(args.LEVEL)));
      return this.call('sys.volume.set', { level }).then(() => undefined, () => undefined);
    }
    async changeVolume(args) {
      const v = await this.soft('sys.volume', {}, null);
      if (!v) return;
      const level = Math.max(0, Math.min(100, v.level + Scratch.Cast.toNumber(args.DELTA)));
      await this.call('sys.volume.set', { level }).catch(() => {});
    }
    async volume() {
      const v = await this.soft('sys.volume', {}, null);
      return v ? v.level : -1;
    }
    setMute(args) {
      const muted = Scratch.Cast.toString(args.STATE) !== 'unmute';
      return this.call('sys.volume.set', { muted }).then(() => undefined, () => undefined);
    }
    async muted() {
      const v = await this.soft('sys.volume', {}, null);
      return !!(v && v.muted);
    }
    mediaControl(args) {
      return this.call('sys.media.control', { action: Scratch.Cast.toString(args.ACTION) })
        .then(() => undefined, () => undefined);
    }
    async nowPlaying(args) {
      const m = await this.soft('sys.media.info', {}, null);
      if (!m || !m.present) return '';
      const field = Scratch.Cast.toString(args.FIELD);
      return field in m ? m[field] : '';
    }

    openUrl(args) {
      return this.call('sys.open_url', { url: Scratch.Cast.toString(args.URL) }).then(() => undefined, () => undefined);
    }
    notify(args) {
      return this.call('notify', { title: Scratch.Cast.toString(args.TITLE), body: Scratch.Cast.toString(args.BODY) })
        .then(() => undefined, () => undefined);
    }
    clipWrite(args) {
      return this.call('clipboard.write', { text: Scratch.Cast.toString(args.TEXT) }).then(() => undefined, () => undefined);
    }
    async clipRead() {
      const r = await this.soft('clipboard.read', {}, null);
      return r ? r.text : '';
    }
    async processes() {
      const r = await this.soft('sys.processes', { limit: 100 }, { processes: [] });
      return JSON.stringify(r.processes || []);
    }
    killPid(args) {
      return this.call('sys.kill', { pid: Number(args.PID) || 0 }).then(() => undefined, () => undefined);
    }
    power(args) {
      return this.call('sys.power', { action: Scratch.Cast.toString(args.ACTION) }).then(() => undefined, () => undefined);
    }
    async isAdmin() {
      const r = await this.soft('sys.elevation', {}, null);
      return !!(r && r.elevated);
    }
    requestAdmin() {
      return this.call('sys.elevate', {}).then(() => undefined, () => undefined);
    }

    async raw(args) {
      let params = {};
      try {
        params = JSON.parse(Scratch.Cast.toString(args.PARAMS) || '{}');
      } catch (e) {
        this.lastError = `params is not JSON: ${e.message}`;
        return '';
      }
      const r = await this.soft(Scratch.Cast.toString(args.METHOD), params, null);
      return r === null ? '' : JSON.stringify(r);
    }
  }

  Scratch.extensions.register(new Conduit());
})(Scratch);
