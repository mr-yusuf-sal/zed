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

## 2–7. adımlar (squash/reword, branch/tag, durum şeridi, sürükle-bırak, filtreler, otomatik fetch)

Tasarım kararları:
- **Squash:** Seçim HEAD'den geriye doğru tek bir düz çizgi olmalı (her commit'in tek parent'ı bir sonraki seçili commit). Ayrıca staged değişiklik olmamalı. Kurallar `squash_base`'te; uymayan durumda panel butonu pasif ve tooltip'te nedeni yazıyor. Akış: commit mesajları eskiden yeniye birleştirilip `CommitMessageModal`'a (`git_ui.rs`, çok satırlı, ctrl/cmd+enter onaylar) dolduruluyor. Onaylanınca `Repository::reset(<en eskinin parent'ı>, Soft)` ve ardından `Repository::commit` çalışıyor. Commit adımı başarısız olursa hata mesajı değişikliklerin staged kaldığını ve `git reset --soft ORIG_HEAD` ile geri alınabileceğini söylüyor. Interactive rebase yok, o yüzden HEAD'e ulaşmayan aralıklar kapsam dışı.
- **Reword:** Sadece HEAD için ve staged değişiklik yokken; `CommitOptions { amend: true }` ile commit. Commit menüsünde ("Reword Commit…"), sağ tıklanan commit üzerinde çalışıyor.
- **Branch/tag yönetimi** (ref etiketine sağ tık, `ref_management_menu_entries`):
  - Local branch: Rename…, Push to `<remote>`, Set Upstream to `<remote>/<ad>`, Delete Branch, Force Delete Branch…. Rename için mevcut `RenameBranchModal` kullanılıyor (`rename_branch_modal`). Upstream seçeneği sadece aynı adda bir remote branch varsa ve zaten o takip edilmiyorsa çıkıyor. Silme seçenekleri checked-out branch'te pasif.
  - Remote branch: Delete from `<remote>`… (`git push <remote> --delete`).
  - Tag: Push Tag to `<remote>`, Delete Tag…, Delete Tag from `<remote>`….
  - Hepsi terminal task. Silmeler onay soruyor. Remote listesi `branch_list` içindeki remote branch'lerden çıkarılıyor, yani hiç branch'i olmayan bir remote listede yok.
- **Durum şeridi:** `OperationInProgress::detect`, `repository_dir_abs_path` altında `rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD` arıyor. Bunu projenin `Fs`'i ile yapıyor, bu yüzden testlerde FakeFs ile de çalışıyor. Graph oluşturulurken ve graph yükleme olayları dışındaki her repository olayında yeniden kontrol ediliyor. Şerit arama çubuğunun altında: Continue (`git -c core.editor=true <işlem> --continue`, mesaj editörü terminalde beklemesin diye), Skip (merge hariç), Abort (onaylı).
- **Sürükle-bırak:** Branch etiketleri (tag ve HEAD hariç) sürüklenebiliyor. Tablo satırları ve graph sütunu bırakma hedefi. Bırakınca menü açılıyor: HEAD'e bırakıldıysa "Merge `<A>` into `<current>`"; local branch için "Rebase `<A>` onto `<hedef>`" (`git rebase <sha> <A>`); A mevcut branch değilse "Move `<A>` Here" (`git branch -f`), mevcut branch ise "Reset `<A>` to Here…" (hard, onaylı). Kendi commit'ine bırakmak bir şey yapmıyor.
- **Branch filtresi:** Arama çubuğunda "All Branches / `<branch>`" açılır menüsü (`render_branch_filter`, `set_log_source`). Kaynak değişince seçim temizleniyor, çünkü seçim satır indeksi olarak tutuluyor. File history modunda menü gösterilmiyor.
- **Arama niteleyicileri:** `CommitSearchFilters::parse` (`crates/git/src/repository.rs`). `author:`, `since:`, `until:` tek kelime ya da çift tırnaklı ifade alıyor ve `--author`, `--since`, `--until` olarak `git log`'a ekleniyor. Kalan metin `--grep` oluyor. Niteleyici yoksa sorgu eskisi gibi olduğu gibi aranıyor, hash araması da eskisi gibi çalışıyor. `--fixed-strings` ve büyük/küçük harf ayarı `--author`'a da uygulanıyor. Bu bir filtre değil, eşleşenleri vurgulama ve aralarında gezme.
- **Otomatik fetch:** Fetch butonunun solundaki saat ikonu ile açılıp kapanıyor; durum KVP'de (`git_graph_auto_fetch`) saklanıyor. Açıkken graph her 5 dakikada bir `Repository::fetch(FetchOptions::All, …)` çağırıyor. Toast yok. Askpass boş, yani şifre isteyen remote sessizce başarısız olup bir sonraki tura kalıyor; hata sadece debug log'a düşüyor. Repository'yi doğrudan çağırdığı için aktif repo kısıtı yok. Graph kapanınca task da düşüyor.

Bilinen sınırlamalar:
- Bunların hiçbiri çalışan bir Zed'de denenmedi.
- Squash/reword dışındaki her şey terminal task. Komut başarısız olursa terminalde açık kalıyor, ama graph bunu ayrıca bildirmiyor.
- Arama niteleyicileri collab'da host'un da bu kodu çalıştırmasını gerektiriyor. Fake repo niteleyicileri tanımıyor, o yüzden sadece ayrıştırıcının birim testi var.
- Remote bir projede durum şeridi yerel `Fs` ile bakamayacağı için çıkmayabilir.
- Otomatik fetch her graph sekmesi için ayrı çalışıyor; aynı repoya iki graph açıksa iki kat fetch olur.

## Kontrol listesi
- `README.md` başındaki `[!IMPORTANT]` satırları zaten var, dokunma.
- `cargo fmt -p git_ui -p git`, `cargo clippy -p git_ui --all-targets`, `cargo test -p git_ui --lib`, `cargo test -p git --lib commit_search`.
