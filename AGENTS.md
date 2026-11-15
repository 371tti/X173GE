# X173GE 作業規約

- rebuild1 の設計とアルゴリズムを参考にし、コードは移植せず書き直す。特にGLB/glTFの読み込みはKhronos仕様を唯一の形式基準にし、rebuild1のloaderから形式解釈や実装を引き継がない。`../rebuild1/assets/model.glb` は無変更の検証fixtureとしてのみ使う。
- 変更前に再現条件、所有者、データの流れを調べる。根本原因と同じ不変条件を共有する関連経路をまとめて直す。原因が分からない場合は観測を増やし、症状だけを隠す workaround を恒久化しない。
- 必要な不変条件を最小の構造で守る。抽象化は重複か複雑さを実際に減らす場合だけ導入する。実装行数、実行時コスト、理解・保守コストを同時に抑える。コードゴルフや説明しづらい圧縮はしない。
- Vulkan object は renderer thread と Vulkan crate に閉じ込める。ゲーム/ECS 側との境界は bounded message と owned `FrameSnapshot` にする。surface ID と generation は protocol の境界で検証する。
- 遅い処理を bounded channel から取り出した後に無制限 queue へ移さない。並列数と pending 数に上限を置き、shutdown も必ず完了できる経路にする。
- ECS storage と scheduler は Shipyard に任せる。X173GE 側に追加するのは必要な engine API と render extraction の接続だけ。
- 描画機能の実装前に原典を読み、`docs/research/` に出典の種類、採用 variant、式、前提、逸脱、検証 scene を記録する。論文、技術メモ、talk、production reference を混同しない。
- 論文調査、root-cause review、binary/dependency audit は最大3つの独立 agent に分担する。agent が同じファイルを同時編集しない。主担当が共通 interface と統合を管理する。
- 依存は必要な機能だけ有効化する。binary と配布物のサイズを標準 build と `high-quality` build ごとに実測する。見積りで依存を外さない。
- Windows を先に実装・検証し、Linux は X11/Wayland の境界を保ったうえで後段に追加する。audio と editor は初期範囲に含めない。
- 変更後は元の再現条件と関連経路を確認する。GPU を使わない unit test は protocol、型制約、時間制御など、正誤を自動判定できる境界に絞る。描画品質は validation smoke と visual review で確認する。
- 描画機能ごとに、自動 smoke の後で実モデルを使った人間の visual review を挟む。品質判断は smoke の合格だけで完了扱いにしない。
