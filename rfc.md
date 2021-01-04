# Proposal:  Userspace storage for TiKV

- Author(s): Yang Keao, Yinghao Wang
- Last updated: Mon Jan  4 07:02:56 PM CST 2021
- Discussion at: 

## Abstract

In this proposal, we need to adopt the userspace NVMe driver and other tools provided by [SPDK](https://spdk.io/) to TiKV, and test the robustness and performance. 

We hope to get a significant performance improvement, and bring us more opportunity and customizability to schedule the IO task. This proposal will also get us more prepared for the NVDIMM storage device, which is obviously the future of storage.

## Background

The implementation and performance of storage layer (RocksDB) highly depends on the Linux kernel. However, with the NVMe device widely used, the kernel storage layer implementation: from the syscall interrupt, to file system layers, has been a limitation of the storage performance. And the IOMMU and hugepage features in linux kernel make it possible to get a userspace driver for some hardware (like ethernet device and storage device).

There has been a lot of inspectation on userspace drivers. All of them has shown a significant improvement. For example, the [EvFS](https://www.usenix.org/system/files/hotstorage19-paper-yoshimura.pdf) has shown 5~20 times improvement on different metrics. Intel, the company who has developed SPDK and the most famous NVDIMM device Optane, also provides a delightful benchmark on SPDK.

From another aspect, with the implementation in kernel, we cannot fully control the running state of TiKV, which means our users may face degradation on a customized or lower version of kernel. We have noticed that the upgrade of kernel will bring us a performance improvement, vive versa.

By using userspace storage implementation, we can use algorithms and filesystems which are more suitable for NVMe. And by taking control of IO tasks from kernel, TiKV can be more controllable and be prepared for more detailed scheduling. And the 2020~2021 is also a good time to carry on this proposal, as the software (SPDK) and technology has been tested in many different projects, and the hardware (both NVMe and NVDIMM devices) gets more cost-effective.

## Proposal

The `Env` abstraction of `Rocksdb` makes it easy to adopt a new storage implementation (or storage environment). 

The `rocksdb` used by `TiKV` should be modified to integrate with a userspace storage environment (e.g. blobfs). And a binding should also be written for rust in `rust-rocksdb`.

The initialization steps of TiKV should also be modified to use a customized environment rather than the default system one.

## Compatibility and Migration Plan

We should provide a migration tool to move data from a normal filesystem into the raw disk. A seamlessly migration tool could be better, if we can fill the gap between two implementation: falling back to the normal filesystem if the file cannot be found in userspace driver, and creating new sst files in the new driver.

The WAL should always be in a normal filesystem, as it's hard to support `mmap` operation: though userspace pagefault handler is possible, marking dirty page and flash back is not that easy (at least for now).

## Implementation

This proposal can be split into two parts:

### Provide options to use userspace driver

Modify `rust-rocksdb` and `rocksdb` to use `blobfs` provided by `SPDK`. Some modifications on the `blobfs` should be done to fit in the TiKV senerio: multiple `rocksdb` cluster.

After implementation, some easy optimization could be done for this file system. 

@YangKeao willl handle this task.

### Provide migration tools/mechanism

A tool should also be provided to drop down the cost of migrating from a normal filesystem to the newly provided storage driver.

@Yinghao Wang will handle this task.

## Testing Plan

By setting up TiKV with a userspace storage driver and running normal tests and benchmarks.



