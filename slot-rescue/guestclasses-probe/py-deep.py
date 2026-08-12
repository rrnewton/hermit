# Deep python3 workload: hashlib, json, itertools, file I/O, decimal, re.
import hashlib, json, os, re, tempfile, itertools, decimal
with tempfile.TemporaryDirectory(dir='.') as d:
    names=[os.path.join(d,f'f{i}.txt') for i in range(16)]
    for i,n in enumerate(names): open(n,'w').write('x'*(1024*(i+1)))
    h=hashlib.sha256(); total=0
    for n in names:
        b=open(n,'rb').read(); total+=len(b); h.update(b)
    obj={'total':total,'count':len(names),'digest':h.hexdigest()}
    assert json.loads(json.dumps(obj))==obj
    primes=[p for p in range(2,200) if all(p%q for q in range(2,int(p**.5)+1))]
    s=sum(decimal.Decimal(1)/decimal.Decimal(p) for p in primes)
    assert re.fullmatch(r'[0-9a-f]{64}', obj['digest'])
    print(f"py-deep:{obj['count']}:{obj['total']}:{obj['digest']}:{len(primes)}:{str(s)[:12]}")
