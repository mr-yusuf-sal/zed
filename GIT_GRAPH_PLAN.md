# Git Graph geliştirmeleri — durum ve plan

Bu dosya fork'a özel bir çalışma notudur (upstream'e gitmez). İş bitince silinebilir.

## Bitenler (`custom` üzerinde, push'lu)

| Commit | İçerik |
|---|---|
| `a121ebb447` | Detay paneli yerleşimi: tek satırlık başlık (avatar + yazar/tarih + butonlar), e-posta/SHA/"View on" yan yana sarılan satırda, alt konumda detaylar solda / dosyalar sağda iki sütun. |
| `93aad1e861` | Shift+tık ve Shift+↑/↓ ile aralık seçimi ve "N commits selected" paneli (Cherry-pick / Revert / Copy SHAs). Commit ve ref menülerine Checkout, Create Branch Here, Merge, Rebase, Cherry-pick, Revert, Reset (Soft/Mixed/Hard + onay) eklendi. Arama çubuğuna Fetch / Pull / Push butonları eklendi (ahead/behind sayılarıyla). |
| (bu commit) | Ctrl/Cmd+tık ile commit ekleyip çıkarma ve tam 2 commit seçiliyken "Compare" (panelde buton, menüde "Compare Commits"). Karşılaştırma `CommitView::open_comparison` ile açılıyor ve eski commit'ten yenisine ne değiştiğini gösteriyor. |

Kullanılan tasarım kararları:
- Merge / rebase / cherry-pick / revert / reset **terminal task'ı** olarak çalışıyor (`run_git_in_terminal`, `git_graph.rs`). Sebebi: `Repository`'de bu işler için job yok ve bunlar conflict'te duran komutlar. Terminal başarı olunca kapanıyor (`HideStrategy::OnSuccess`), hata/conflict'te açık kalıyor.
- Fetch/Pull/Push butonları `git::Fetch` / `git::Pull` / `git::Push` aksiyonlarını dispatch ediyor. Bu aksiyonlar git panel'in **aktif repo**'su üzerinde çalışıyor, o yüzden graph başka bir repoyu gösteriyorsa butonlar pasif.
- "Create Branch Here", `CreateTagModal`'ı kullanıyor. Modal artık `CreateRefKind { Tag, Branch }` alıyor (`git_ui.rs`). Branch'i `git switch -c` ile oluşturduğu için branch'e de geçiyor.
- Commit menüsüne dışarıdan entry eklemek için `CommitContextMenuData.extra_entries` eklendi (`commit_context_menu.rs`).
- Seçim modeli aralık değil küme: `multi_selected_idxs: Rc<BTreeSet<usize>>` + `selection_anchor_idx`. Boş küme = sadece `selected_entry_idx` seçili, böylece tek commit'lik kod yolları değişmedi. Ctrl/Cmd+tık satırı ekleyip çıkarıyor ve anchor'ı o satıra taşıyor; Shift+tık anchor'dan tıklanan satıra aralığı yeniden kuruyor (ctrl ile eklenmiş diğer satırlar düşüyor, dosya listelerindeki gibi).
- Karşılaştırmada "eski" ve "yeni" graph'taki sıraya göre: alttaki satır eski. Fark `git diff <eski> <yeni>` ile iki commit'in ağaçları arasında alınıyor; aradaki geçmişe ya da merge-base'e bakılmıyor.
- Backend: `RealGitRepository::load_commit` artık `eski..yeni` biçimindeki string'i görünce (`split_revision_range`, `repository.rs`) `git show` yerine `git diff -z --no-renames --raw --no-abbrev <eski> <yeni> --` çalıştırıyor; raw çıktı aynı olduğu için ayrıştırma ve cat-file kısmı değişmedi. `git show A..B` aralıktaki her commit'in değişikliğini art arda bastığı için (aynı dosya birden çok kez çıkıyor) kullanılamazdı. Aralıkta shallow kontrolü backend'de atlanıyor: `rev-parse --verify A..B^{commit}` shallow bir klonda "Needed a single revision" hatası veriyor (shallow olmayan repoda `shallow` dosyası olmadığından kontrol zaten rev-parse'a gelmeden dönüyordu). UI da `ignore_shallow_boundary = true` geçiyor. `A...B` ve ucu boş aralıklar bölünmüyor, eskisi gibi `git show`'a gidiyor. Remote (collab) tarafında string proto'dan olduğu gibi geçiyor, proto değişikliği yok.
- `CommitView`: `older_commit: Option<CommitDetails>` alanı eklendi; `commit` karşılaştırmada yeni commit'in gerçek detayları, yani `commit.sha` her zaman tek bir SHA. Karşılaştırmada ayrı bir başlık çiziliyor ("Changes from abc1234 to def5678" + iki commit'in konusu; avatar, yazar ve SHA kopyalama yok). Sekme başlığı `abc1234..def5678`, tooltip'te iki commit'in konusu, dosya buffer adları `abc1234..def5678 - dosya`. Mevcut sekmeyi bulma `revision()` ile yapılıyor, böylece aynı karşılaştırma tekrar açılınca aynı sekme yenileniyor ve yeni commit tek başına açılınca karşılaştırma sekmesinin yerine geçmiyor. Toolbar'da "View on <provider>" gizli (permalink tek commit'e gidiyor), "Show in Git Graph" yeni commit'i açıyor. "Open File in Project" çalışma ağacındaki dosyayı açtığı için değişmedi.

Bilinen sınırlamalar:
- Remote branch'te "Checkout", remote öneki olmadan `git switch <ad>` çalıştırıyor. Aynı adda ama başka yerde duran bir local branch varsa, menünün açıldığı commit'e değil ona geçer.
- Menüler, terminal task'ları ve toolbar henüz çalışan bir build'de denenmedi (clippy + git_ui testleri geçiyor).
- Ctrl+tık seçimi ve karşılaştırma görünümü de çalışan bir Zed'de denenmedi; başlığın ve sekmenin görünüşü sadece koddan biliniyor. Testler seçim mantığını, sekmenin yeniden kullanılmasını (fake repo, boş diff) ve gerçek git ile `load_commit`'in aralık davranışını kapsıyor.
- Collab'da host da bu kodu çalıştırmalı: eski bir host `A..B`'yi hâlâ `git show`'a verir ve yanlış (art arda) diff döner. Collab yolu test edilmedi.
- `git diff` veya `show` başarısız olursa karşılaştırma sekmesi hiç açılmıyor ve hata sadece log'a düşüyor (tek commit açma yolu da böyle davranıyor).
- `FakeGitRepository::load_commit` her string için boş diff döndüğünden değiştirilmedi; fake repo'da karşılaştırmanın dosya içeriği test edilemiyor.

## Yapılacaklar

### 2) Squash ve reword
- **Squash:** Seçili commit'ler HEAD'den geriye doğru ardışık ve doğrusal olmalı (her birinin parent'ı bir sonraki). Değilse menü pasif ve tooltip'te nedeni yazsın. Akış: mesajları birleştirip önceden dolduran çok satırlı bir pencere açılır, onaylanınca `Repository::reset(<en eski>~1, Soft)` ve ardından `Repository::commit(message, None, CommitOptions::default(), askpass, cx)` çalışır.
- **Reword:** Sadece HEAD için: mesaj penceresi açılır, onaylanınca `commit` amend ile çalışır (`CommitOptions { amend: true, .. }`). HEAD dışındaki commit'ler interactive rebase gerektirir, kapsam dışı.
- Pencere: `CreateTagModal` benzeri, ama `Editor::multi_line` kullanan yeni bir modal.

### 3) Branch / tag yönetimi (ref etiketine sağ tık)
- Branch silme: `Repository::delete_branch(is_remote, name, force)`. Merge edilmemişse force için onay sorulsun.
- Branch'i yeniden adlandırma: `RenameBranchModal` (`git_ui.rs`) şu an aktif branch adını alıyor. Herhangi bir branch için kullanılabilir mi bakılmalı, `rename_branch(branch, new_name)` zaten var.
- Branch push etme: terminal task olarak `git push -u <remote> <branch>`. Remote listesi `get_remotes`'tan veya `branch_list` içindeki remote adlarından alınabilir.
- Upstream ayarlama: `git branch --set-upstream-to=<remote>/<branch> <branch>`. Sadece eşleşen remote branch varsa gösterilsin.
- Tag silme: `git tag -d <tag>` (terminal veya `Repository`'de ref silme varsa o). Tag push etme: `git push <remote> <tag>`.

### 4) İşlem sürerken çıkan şerit
- `RepositorySnapshot.merge: MergeDetails` sadece merge mesajı ve conflict'li path'leri veriyor. Rebase / cherry-pick / revert tespiti için `dot_git_abs_path` altında şunlara bakılmalı: `rebase-merge/`, `rebase-apply/`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `MERGE_HEAD`. Repo değişince (repository event) yeniden kontrol edilmeli.
- Graph'ın üstünde bir şerit: "Rebase sürüyor — Continue / Skip / Abort". Butonlar terminal task olarak `git <işlem> --continue|--skip|--abort` çalıştırsın. Merge için Skip yok, Continue ise `git merge --continue`.

### 5) Sürükle-bırak
- `render_ref_chip` içindeki etikete `.on_drag(DraggedRef { name, sha }, …)` eklenmeli. Tablo satırlarına (`map_row`) ve graph canvas'ına `.on_drop` eklenmeli.
- Bırakınca açılan menü:
  - "Rebase `<A>` onto `<hedef>`" → `git rebase <hedef> <A>`
  - "Merge `<A>` into `<current>`" (hedef mevcut branch ise)
  - "Move `<A>` here" → `git branch -f <A> <sha>` (A mevcut branch değilse; mevcut branch ise hard reset onayı)

### 6) Filtreler
- **Branch filtresi:** `LogSource::Branch(..)` zaten var ve proto'da destekleniyor. Toolbar'a "All branches / <branch>" seçici eklenmeli: `self.log_source` değişir, ardından `invalidate_state` ve `fetch_initial_graph_data` çalışır. Graph'ın log_source değişimine doğru tepki verdiği doğrulanmalı. Seçim workspace serileştirmesine zaten giriyor (`LOG_SOURCE_BRANCH`).
- **Yazar / tarih:** Arama şu an `git log --grep` kullanıyor (`search_commits`, `repository.rs` ~3464). Sorgu içindeki `author:<ad>`, `since:<tarih>`, `until:<tarih>` niteleyicileri ayrıştırılıp `--author`, `--since`, `--until` olarak eklenmeli. Sorgu string olarak geçtiği için proto değişikliği gerekmez. Bu bir filtre değil, eşleşenleri vurgulama/gezme olur: graph çizgileri tüm commit'lere göre hesaplandığı için gerçek gizleme mümkün değil.

### 7) Otomatik fetch
- Toolbar'da aç/kapa butonu, durumu KVP'de saklanır (`commit_details_position` gibi).
- Açıkken graph her 5 dakikada bir `repository.fetch(FetchOptions::All, AskPassDelegate::new(cx, |_, _, _| {}), cx)` çağırır. Toast gösterilmez. Şifre istenirse askpass sessizce düşer ve fetch başarısız olur. Timer task'ı bir alanda tutulur, graph kapanınca iptal olur.
- Fetch butonlarından farklı olarak repository'yi doğrudan çağırdığı için aktif repo kısıtı yok.

## Kontrol listesi (her adım için)
- `README.md` başındaki `[!IMPORTANT]` satırları zaten var, dokunma.
- `cargo fmt -p git_ui`, `cargo clippy -p git_ui --all-targets`, `cargo test -p git_ui --lib`.
- Ayrı commit, mesaj formatı mevcut commit'ler gibi. Ardından `origin/custom`'a push.
