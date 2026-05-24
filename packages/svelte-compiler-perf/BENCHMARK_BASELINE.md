# Compile benchmark baseline (svelte-compiler-perf)

Generated: 2026-05-24T08:57:01.106Z

## End-to-end (sandbox vs upstream, client, 1000 iter)

### client

```
fixture                                                 | LOC | compiler | ms/iter | vs upstream
-----------------------------------------------------------------------------------------------
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |    sandbox |     0.0913 |        1.14x
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |   upstream |     0.0797 |        1.00x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |    sandbox |     0.7787 |        1.05x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |   upstream |     0.7413 |        1.00x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |    sandbox |     0.2355 |        1.05x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |   upstream |     0.2249 |        1.00x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |    sandbox |     0.4777 |        1.08x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |   upstream |     0.4438 |        1.00x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |    sandbox |     0.0457 |        1.24x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |   upstream |     0.0369 |        1.00x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |    sandbox |     0.0643 |        0.90x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |   upstream |     0.0712 |        1.00x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |    sandbox |     0.2254 |        1.07x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |   upstream |     0.2104 |        1.00x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |    sandbox |     0.0887 |        1.01x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |   upstream |     0.0882 |        1.00x

Mean sandbox/upstream: 1.067x (iterations=1000, mode=client)
```

### server

```
fixture                                                 | LOC | compiler | ms/iter | vs upstream
-----------------------------------------------------------------------------------------------
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |    sandbox |     0.0696 |        1.19x
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |   upstream |     0.0586 |        1.00x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |    sandbox |     0.7135 |        1.04x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |   upstream |     0.6856 |        1.00x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |    sandbox |     0.2573 |        1.07x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |   upstream |     0.2408 |        1.00x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |    sandbox |     0.4174 |        1.10x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |   upstream |     0.3800 |        1.00x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |    sandbox |     0.0395 |        1.22x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |   upstream |     0.0325 |        1.00x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |    sandbox |     0.0456 |        1.06x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |   upstream |     0.0430 |        1.00x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |    sandbox |     0.1469 |        1.03x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |   upstream |     0.1426 |        1.00x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |    sandbox |     0.0809 |        1.05x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |   upstream |     0.0770 |        1.00x

Mean sandbox/upstream: 1.094x (iterations=1000, mode=server)
```

## Per-phase (sandbox, 2000 iter)

### hello-world (client)

```
fixture: packages/svelte/tests/snapshot/samples/hello-world/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0053
analyze   | 0.0236
transform | 0.0439
codegen   | 0.0468
e2e       | 0.0469
```

### hello-world (server)

```
fixture: packages/svelte/tests/snapshot/samples/hello-world/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0052
analyze   | 0.0238
transform | 0.0358
codegen   | 0.0362
e2e       | 0.0384
```

### skip-static-subtree (client)

```
fixture: packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0589
analyze   | 0.2000
transform | 0.4837
codegen   | 0.5247
e2e       | 0.6159
```

### skip-static-subtree (server)

```
fixture: packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0588
analyze   | 0.2033
transform | 0.4632
codegen   | 0.4748
e2e       | 0.5528
```

### props-identifier (client)

```
fixture: packages/svelte/tests/snapshot/samples/props-identifier/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0229
analyze   | 0.0504
transform | 0.1585
codegen   | 0.1668
e2e       | 0.1904
```

### props-identifier (server)

```
fixture: packages/svelte/tests/snapshot/samples/props-identifier/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0229
analyze   | 0.0499
transform | 0.1673
codegen   | 0.1786
e2e       | 0.2020
```

### function-prop-no-getter (client)

```
fixture: packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0500
analyze   | 0.0678
transform | 0.2832
codegen   | 0.3103
e2e       | 0.3420
```

### function-prop-no-getter (server)

```
fixture: packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0508
analyze   | 0.0698
transform | 0.2319
codegen   | 0.2444
e2e       | 0.2843
```
