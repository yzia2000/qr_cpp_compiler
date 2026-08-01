# Cross-compiler validation

- reference: gcc-strict, 298,720 quotes, `quotes_dev.pcap`
- tolerances: strict IV <= 1e-09, fast IV <= 5e-05 vol pts

- **gcc-fast**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.59e-08
- **gcc-fast-zmm**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.59e-08
- **clang-strict**: OK — max|dIV| 0.00e+00, conv flips 0.00e+00, max|dSVI| 0.00e+00
- **clang-fast**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.71e-08
- **clang-fast-zmm**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.71e-08
- **icpx-strict**: OK — max|dIV| 1.90e-14, conv flips 0.00e+00, max|dSVI| 1.10e-08
- **icpx-fast**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.30e-08
- **icpx-fast-zmm**: OK — max|dIV| 2.01e-14, conv flips 0.00e+00, max|dSVI| 1.59e-08

## Fit vs ground truth (gcc-strict)
- slices fitted: 512/512
- mean |rho error|: 0.0694
- mean fit RMSE (total var): 3.28e-04
