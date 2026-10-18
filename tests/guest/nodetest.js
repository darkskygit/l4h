// Exercise V8 string internalization and concurrent GC, plus fork/exec startup.
// Run with --expose-gc --max-old-space-size=128.
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const { Worker, isMainThread } = require('node:worker_threads');

function strings() {
  for (let round = 0; round < 50; round++) {
    const objects = [];
    for (let i = 0; i < 4000; i++) {
      const key = (`prefix:${round}:${i}:` + 'abcdef'.repeat(100)).slice(7);
      const value = JSON.parse(JSON.stringify({ [key]: i, [key + ':tail']: round }));
      assert.equal(value[key], i);
      assert.equal(value[key + ':tail'], round);
      objects.push(value);
    }
    global.gc();
  }
}

async function main() {
  console.log(`NODETEST: start ${process.version}`);
  for (let i = 0; i < 20; i++) {
    const result = spawnSync(process.execPath, ['-e',
      'let s=0; for(let i=0;i<1e6;i++) s+=Math.sqrt(i); ' +
      'if(Math.abs(s-666666166.4588418)>1e-3) process.exit(1);']);
    assert.ifError(result.error);
    assert.equal(result.status, 0, `startup ${i}: signal=${result.signal} ${result.stderr}`);
  }
  const workers = Array.from({ length: 4 }, () => new Promise((resolve, reject) => {
    const worker = new Worker(__filename);
    worker.on('error', reject);
    worker.on('exit', code => code === 0 ? resolve() : reject(new Error(`worker exit ${code}`)));
  }));
  strings();
  await Promise.all(workers);
  console.log('NODETEST: PASS');
}

if (isMainThread) {
  main().catch(error => { console.error('NODETEST: FAIL', error); process.exit(1); });
} else {
  strings();
}
