// Deep node workload: libuv threadpool (async fs), crypto, JSON, Buffer.
const fs=require('fs').promises, crypto=require('crypto'), path=require('path'), os=require('os');
(async()=>{
  const d=await fs.mkdtemp('nodedeep-');
  const names=[...Array(16).keys()].map(i=>path.join(d,`f${i}.txt`));
  // Parallel writes then parallel reads -> genuinely uses the libuv threadpool.
  await Promise.all(names.map((n,i)=>fs.writeFile(n, 'x'.repeat(1024*(i+1)))));
  const bufs=await Promise.all(names.map(n=>fs.readFile(n)));
  const total=bufs.reduce((a,b)=>a+b.length,0);
  const h=crypto.createHash('sha256'); bufs.forEach(b=>h.update(b));
  const obj={total, count:bufs.length, digest:h.digest('hex')};
  const round=JSON.parse(JSON.stringify(obj));
  await Promise.all(names.map(n=>fs.unlink(n))); await fs.rmdir(d);
  if(round.total!==total) throw new Error('json roundtrip');
  console.log(`node-deep:${round.count}:${round.total}:${round.digest}`);
})().catch(e=>{console.error(e);process.exit(1)});
