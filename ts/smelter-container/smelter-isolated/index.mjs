import ivm from 'isolated-vm';

// const snapshot = Isolate.createSnapshot([
//   {
//     code:
//       `
//       console.log("test");
//       `
//   }
// ]);

const isolate = new ivm.Isolate({
  memoryLimit: 128,
  inspector: false,
  // snapshot,
  onCatastrophicError: (msg) => {
    console.error("horrible thing has happened:", msg);
  }
});

const context = isolate.createContextSync();
const jail = context.global;
jail.setSync('_console_log', console.log);
jail.setSync('_console_warn', console.warn);
jail.setSync('_console_error', console.error);
jail.setSync('_console_table', console.table);

const prelude = isolate.compileScriptSync(`
  globalThis.console = {
    log: _console_log,
    warn: _console_warn,
    error: _console_error,
    table: _console_table,
  };
`);

prelude.runSync(context);
const script = isolate.compileScriptSync(`
  console.log("test");
  console.warn("test");
  console.error("test");
  console.table(["test"]);
`);

script.runSync(context)
