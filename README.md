# rp2350-VVVF-BLDC

RP2350 + DRV8313 + アウトランナーBLDCで、電車のVVVFインバータ風サウンドを出しながらモーターを回すサンプルです。  
ボリューム(可変抵抗)をマスコンとして使います。

## 構成

- MCU: RP2350 (Raspberry Pi Pico 2など)
- 3相ドライバ: DRV8313
- モーター: 3相アウトランナーBLDC
- 入力: 10kΩボリューム(ADC)

## 配線例

> 実機に合わせて必ず見直してください。  
> DRV8313側の電源・ゲートドライブ周辺部品・保護回路はデータシート準拠で実装してください。

- `GP2`  -> DRV8313 IN1 (U相)
- `GP3`  -> DRV8313 IN2 (V相)
- `GP4`  -> DRV8313 IN3 (W相)
- `GP5`  -> DRV8313 nSLEEP (H:有効, L:スリープ)
- `GP26`(ADC0) <- ボリューム中央端子
- 3.3V  <- ボリューム片端
- GND   <- ボリューム片端

## ビルド

Rust + Embassyでビルドします。

```bash
rustup target add thumbv8m.main-none-eabihf
cargo build --release
cargo install elf2uf2-rs
elf2uf2-rs target/thumbv8m.main-none-eabihf/release/rp2350-vvvf-bldc rp2350-vvvf-bldc.uf2
```

生成物は `target/thumbv8m.main-none-eabihf/release/rp2350-vvvf-bldc` です。  
生成した `rp2350-vvvf-bldc.uf2` を書き込んでください。

## 動作概要

- 20kHz PWMで3相正弦波を生成
- ボリューム位置を速度指令(電気角周波数)に変換
- V/f制御 + 低次高調波注入 + うなりを加えてVVVF風の音色を再現
- 低速時はnSLEEPを落として停止

## 注意

- 本コードは**音再現重視のオープンループ制御**です。高トルク用途・厳密速度制御には不向きです。
- モーター/電源/配線条件により過電流や発熱の危険があります。必ず安全対策を行ってください。