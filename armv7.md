# 在armv7设备上运行 ckb-cli

### 交叉编译环境

1. 安装 armv7 交叉编译器

    ```shell
   wget http://releases.linaro.org/components/toolchain/binaries/4.9-2017.01/arm-linux-gnueabihf/gcc-linaro-4.9.4-2017.01-x86_64_arm-linux-gnueabihf.tar.xz
   tar Jxvf gcc-linaro-4.9.4-2017.01-x86_64_arm-linux-gnueabihf.tar.xz
   mv gcc-linaro-4.9.4-2017.01-x86_64_arm-linux-gnueabihf /opt/
   export PATH=/opt/gcc-linaro-4.9.4-2017.01-x86_64_arm-linux-gnueabihf/bin:"${PATH}"
    ```

2. 配置 Rust 交叉编译环境

   rustup 安装 armv7 的 target：

   ```
   rustup target add armv7-unknown-linux-gnueabihf
   ```

   修改 ~/.cargo/config （如果没有这个文件，则创建）写入：

   ```
   [target.armv7-unknown-linux-gnueabihf]
   linker = "arm-linux-gnueabihf-gcc"
   ```

### 依赖库

1. openssl

   从 openssl 网站下载源码：

   ```shell
   wget https://www.openssl.org/source/openssl-1.1.1e.tar.gz
   tar zxvf openssl-1.1.1e.tar.gz
   mv openssl-1.1.1e /opt/openssl-1.1.1e-armv7
   ```

   交叉编译：

   ```shell
   cd /opt/openssl-1.1.1e-armv7
   CC=gcc CROSS_COMPILE=arm-linux-gnueabihf- ./config no-asm shared
   ```

   注意：这里要手工修改一下生成的 Makefile，删除两处 `-m64` 的编译选项。

   编译：

   ```shell
   make
   ```

   设置两个环境变量：

   ```shell
   export OPENSSL_LIB_DIR=/opt/openssl-1.1.1e-armv7/
   export OPENSSL_INCLUDE_DIR=/opt/openssl-1.1.1e-armv7/include/
   ```

### 编译

```shell
export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
export CXX_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-g++
cargo build --release --target armv7-unknown-linux-gnueabihf
```

修改 `ckb-sdk/Cargo.toml` 和 `ckb-sdk-types/Cargo.toml` 去掉传递给 `ckb-script` 的 `asm` feature。

修改 `ckb-index/Cargo.toml` 里 `rocksdb` 对应的 `commit id`，改成跟 `ckb 0.30.1` 一样。
 