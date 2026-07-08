---
status: current
updated: __TODAY__
tags: [rust, candle, embedding]
---

# candle で ruri-v3-30m を推論する

candle は Rust 製の機械学習フレームワークで、Python の PyTorch に相当する
テンソル演算とモデルロードの機能を提供する。ONNX を経由せず safetensors
から直接重みを読み込めるため、推論サーバーを軽量な単一バイナリとして
配布できるのが利点である。

ruri-v3-30m は日本語向けの埋め込みモデルで、ModernBert アーキテクチャを
採用している。candle 側の ModernBert 実装は重みキー名に `model.` という
プレフィックスを期待するが、配布されている safetensors にはプレフィックス
が付いていないため、ロード時にキー名をリマップする必要がある。

埋め込みベクトルは256次元に圧縮され、意味検索のための近傍探索に使われる。
