#!/usr/bin/env python3
# Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
"""Fail closed on unexpected ABI exports/native imports or direct ELF dependencies.

This is not a loader-namespace or security proof. The embedding executable and
transitive system runtime dependencies must also be reviewed by the integrator.
"""
import argparse
import re
import resource
import subprocess
import sys

# Lightweight inspection only; readelf/nm inherit the same <=1 GiB AS limit.
_soft, _hard = resource.getrlimit(resource.RLIMIT_AS)
_limit = 1024 * 1024 * 1024
if _soft != resource.RLIM_INFINITY:
    _limit = min(_limit, _soft)
if _hard != resource.RLIM_INFINITY:
    _limit = min(_limit, _hard)
resource.setrlimit(resource.RLIMIT_AS, (_limit, _hard))

EXPORTS = {
    "tikv_expr_abi_version",
    "tikv_expr_context_default",
    "tikv_expr_compile",
    "tikv_expr_eval",
    "tikv_expr_result_get_view",
    "tikv_expr_error_get_view",
    "tikv_expr_program_free",
    "tikv_expr_result_free",
    "tikv_expr_error_free",
    "tikv_expr_borrowed_abi_version",
    "tikv_expr_program_supports_borrowed",
    "tikv_expr_eval_borrowed",
    "tikv_expr_diagnostics_get_view",
    "tikv_expr_diagnostics_free",
}
# These are shared platform ABI/runtime dependencies, NOT privately isolated.
SYSTEM_NEEDED = {
    "libc.so.6", "libm.so.6", "libgcc_s.so.1", "libstdc++.so.6",
    "libpthread.so.0", "libdl.so.2", "librt.so.1", "libz.so.1",
    "ld-linux-x86-64.so.2", "ld-linux-aarch64.so.1",
}
NATIVE_IMPORT = re.compile(
    r"(?:grpc|gpr_|absl::|google::protobuf|google::(?!protobuf)|"
    r"(?:^|\s)(?:SSL_|TLS_|OPENSSL_|CRYPTO_|EVP_|BIO_|BN_|RSA_|EC_|"
    r"X509_|ERR_|PEM_|ASN1_|HMAC_|SHA\d*_|MD5_|AES_|RAND_|d2i_|i2d_))"
)


def command(*args):
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library")
    args = parser.parse_args()
    defined = command("nm", "-D", "--defined-only", "--format=posix", args.library)
    exports = {line.split()[0].split("@")[0] for line in defined.splitlines() if line.strip()}
    undefined = command("nm", "-D", "--undefined-only", "--demangle", args.library)
    forbidden_imports = [line.strip() for line in undefined.splitlines() if NATIVE_IMPORT.search(line)]
    dynamic = command("readelf", "--dynamic", args.library)
    needed = set(re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic))
    print("DEFINED EXPORTS:")
    print("\n".join(sorted(exports)))
    print("DIRECT DT_NEEDED:")
    print("\n".join(sorted(needed)))
    print("UNDEFINED SYMBOLS (review platform/runtime imports):")
    print(undefined, end="")
    problems = []
    if exports != EXPORTS:
        problems.append(f"unexpected exports={sorted(exports - EXPORTS)}, missing={sorted(EXPORTS - exports)}")
    if needed - SYSTEM_NEEDED:
        problems.append(f"unapproved direct shared dependencies={sorted(needed - SYSTEM_NEEDED)}")
    if forbidden_imports:
        problems.append(f"host-interposable native imports={forbidden_imports}")
    if problems:
        print("FAIL: " + "; ".join(problems), file=sys.stderr)
        return 1
    print("PASS: exact C exports; no detected SSL/gRPC/protobuf/abseil imports or private DT_NEEDED.")
    print("NOT PROVEN: transitive platform-runtime isolation or coexistence in the real TiFlash executable.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"ELF audit failed to run: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr)
        sys.exit(2)
