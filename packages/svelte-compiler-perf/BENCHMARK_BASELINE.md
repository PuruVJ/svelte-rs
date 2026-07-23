# Compile benchmark baseline (svelte-compiler-perf)

Generated: 2026-05-24T09:09:21.759Z

## End-to-end (sandbox vs upstream, client, 1000 iter)

### client

```
fixture                                                 | LOC | compiler | ms/iter | vs upstream
-----------------------------------------------------------------------------------------------
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |    sandbox |     0.1037 |        1.26x
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |   upstream |     0.0823 |        1.00x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |    sandbox |     0.9122 |        1.07x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |   upstream |     0.8523 |        1.00x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |    sandbox |     0.2383 |        1.03x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |   upstream |     0.2305 |        1.00x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |    sandbox |     0.4939 |        1.09x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |   upstream |     0.4521 |        1.00x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |    sandbox |     0.0468 |        1.26x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |   upstream |     0.0372 |        1.00x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |    sandbox |     0.0776 |        1.18x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |   upstream |     0.0657 |        1.00x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |    sandbox |     0.2102 |        0.99x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |   upstream |     0.2134 |        1.00x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |    sandbox |     0.0861 |        1.02x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |   upstream |     0.0846 |        1.00x

Mean sandbox/upstream: 1.112x (iterations=1000, mode=client)
```

### server

```
fixture                                                 | LOC | compiler | ms/iter | vs upstream
-----------------------------------------------------------------------------------------------
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |    sandbox |     0.0685 |        1.18x
packages/svelte/tests/snapshot/samples/hello-world/index.svelte |        2 |   upstream |     0.0582 |        1.00x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |    sandbox |     0.7360 |        1.08x
packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte |       50 |   upstream |     0.6796 |        1.00x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |    sandbox |     0.2579 |        1.08x
packages/svelte/tests/snapshot/samples/props-identifier/index.svelte |       11 |   upstream |     0.2398 |        1.00x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |    sandbox |     0.4126 |        1.10x
packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte |       14 |   upstream |     0.3759 |        1.00x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |    sandbox |     0.0384 |        1.20x
packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte |        4 |   upstream |     0.0319 |        1.00x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |    sandbox |     0.0455 |        1.07x
packages/svelte/tests/snapshot/samples/bind-this/index.svelte |        2 |   upstream |     0.0426 |        1.00x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |    sandbox |     0.1475 |        1.00x
packages/svelte/tests/snapshot/samples/purity/index.svelte |        5 |   upstream |     0.1476 |        1.00x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |    sandbox |     0.0852 |        1.13x
packages/svelte/tests/snapshot/samples/each-string-template/index.svelte |        4 |   upstream |     0.0753 |        1.00x

Mean sandbox/upstream: 1.104x (iterations=1000, mode=server)
```

## Per-phase (sandbox, 2000 iter)

### hello-world (client)

```
fixture: packages/svelte/tests/snapshot/samples/hello-world/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0051
analyze   | 0.0238
transform | 0.0415
codegen   | 0.0449
e2e       | 0.0455
```

### hello-world (server)

```
fixture: packages/svelte/tests/snapshot/samples/hello-world/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0053
analyze   | 0.0241
transform | 0.0353
codegen   | 0.0389
e2e       | 0.0539
```

### skip-static-subtree (client)

```
fixture: packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0609
analyze   | 0.2017
transform | 0.4768
codegen   | 0.5098
e2e       | 0.5910
```

### skip-static-subtree (server)

```
fixture: packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0593
analyze   | 0.1984
transform | 0.4664
codegen   | 0.4760
e2e       | 0.5491
```

### props-identifier (client)

```
fixture: packages/svelte/tests/snapshot/samples/props-identifier/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0232
analyze   | 0.0488
transform | 0.1655
codegen   | 0.1661
e2e       | 0.1871
```

### props-identifier (server)

```
fixture: packages/svelte/tests/snapshot/samples/props-identifier/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0230
analyze   | 0.0512
transform | 0.1706
codegen   | 0.1795
e2e       | 0.2022
```

### function-prop-no-getter (client)

```
fixture: packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte (index.svelte) mode=client iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0495
analyze   | 0.0706
transform | 0.2774
codegen   | 0.3071
e2e       | 0.3460
```

### function-prop-no-getter (server)

```
fixture: packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte (index.svelte) mode=server iter=2000
phase     | ms/iter
----------|--------
parse     | 0.0515
analyze   | 0.0677
transform | 0.2321
codegen   | 0.2450
e2e       | 0.2855
```
