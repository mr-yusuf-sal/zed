# Quarry — yapılanlar ve yapılacaklar

Son güncelleme: 2026-10-22

## Yapılanlar

| Commit | Ne yapıldı |
|---|---|
| `811f89c6f7` | Bağlantı satırında yalnızca `+` (New Query) ve `⋯` menüsü kaldı. Refresh, Disconnect, Edit, Set Password ve Delete menüye taşındı. |
| `33d4d4f820` | **Hata:** URL'deki `?authSource=admin` gibi seçenekler veritabanı adına karışıyordu. Uzun ad, `⋯` butonunu panelin dışına itiyordu. Ayrıştırıcı düzeltildi, eski kayıtlar yüklenirken onarılıyor, satırlardaki etiketler artık kısalıyor. |
| `823a0eac2e` | **Madde 1, ağaçta sağ tık menüsü:** Open Data, Select Top 100, Count Rows, New Query Here, Open Definition, Open CREATE Script, Copy Name, Copy Qualified Name, Refresh. CREATE script'leri Postgres, SQL Server, ClickHouse ve MongoDB'de üretiliyor; canlı testlerde gidiş-dönüş doğrulandı. |
| `fc9f1f1004` | **Madde 2-3:** Bağlantılara ortam etiketi (dev, staging, prod). Prod sekmesi kırmızı çerçeveli, prod'da veri değiştirebilecek ifadelerden önce onay soruluyor (`statements::may_write`). Klasörler (katlanabilir) ve Duplicate Connection eklendi. |
| `48f7571dd4` | **Madde 13, Docker'dan keşif:** Paneldeki kutu düğmesi `docker inspect` çıktısından veritabanı konteynerlerini, port ve kimlik bilgileriyle birlikte "Docker" klasörüne ekliyor. |
| `4e282510b4` | **Hata:** İçinde `@`, `:`, `/` ya da `%` geçen MongoDB şifreleri bağlantıyı bozuyordu. Kimlik bilgileri artık `Credential` ile veriliyor. |
| `0379246180` | **Madde 4, sonuçlarda arama:** Araç çubuğundaki kutu tüm hücrelerde arıyor. Türkçe `İ`/`ı` doğru eşleşiyor. Kolon sıralama ve kolon filtresi grid'de zaten vardı. |
| `ce8cfef` | **Madde 5'in sürücü katmanı:** `Driver::preview_page(path, limit, offset)` eklendi ve beş sürücüye uygulandı. Derleniyor, canlı testleri **çalıştırılmadı**. |
| `5e07cac3ba` | **Hata (madde 5'in canlı testlerinde çıktı):** ClickHouse, ORDER BY olmadan tabloyu paralel okuduğu için ilk bloktan (~65.000 satır) sonra aynı OFFSET her seferinde başka satırlar döndürüyordu; önizleme artık `max_threads = 1` ile okunuyor. Postgres, shared_buffers'ın dörtte birinden büyük bir tabloyu başka bir oturum tararken onun kaldığı yerden okumaya başlıyordu; havuz bağlantıları artık `synchronize_seqscans = off` ile açılıyor. `preview_page` için beş motora canlı test eklendi. Postgres, Redis, MongoDB ve ClickHouse'ta çalıştırıldı (docker-compose seed'i elde olmadığından testlerin beklediği veri yeniden kuruldu, eski canlı testler de geçti). SQL Server testi **hiç çalıştırılmadı**. |
| (bu commit) | **Madde 5, Load More:** Panelden açılan önizleme tablonun yolunu saklıyor. Sonuç kesildiyse durum çubuğundaki **Load More** `preview_page(path, 500, satır_sayısı)` çağırıp gelen satırları sona ekliyor. Kolonlar adla birleştiriliyor: yeni kolon sona ekleniyor, eski satırlarda NULL oluyor; `edit_target` ve düzenlenmekte olan hücre aynı hücreyi göstermeye devam ediyor. Durum çubuğu "truncated at 2000" yerine "(truncated)" yazıyor; önizleme 500'de kesildiği için bu yanlıştı. GPUI testi sahte bir sürücüyle iki sayfa ekletip doğruluyor. Uygulamanın içinde elle **denenmedi**. |

## Yapılacaklar

### 6. EXPLAIN / sorgu planı
- `Driver::explain(statement, database, analyze: bool) -> Result<Vec<ResultSet>>` eklenecek.
- Postgres: `EXPLAIN` veya `EXPLAIN (ANALYZE, BUFFERS)`.
- SQL Server: ayrı batch'lerde `SET SHOWPLAN_ALL ON` / `OFF`. Analyze için `SET STATISTICS PROFILE ON`.
- ClickHouse: `EXPLAIN indexes = 1`.
- MongoDB: `{"explain": <komut>, "verbosity": "queryPlanner" | "executionStats"}`.
- Explain Analyze ifadeyi gerçekten çalıştırır. Bu yüzden yalnızca `may_write` false olduğunda sunulmalı.
- Araç çubuğunda Explain menüsü olacak, plan grid'de satır satır gösterilecek.

### 7. Satır ekleme ve silme
- `EditTarget`'a `insert_columns: Vec<usize>` alanı eklenecek. Yeni sürücü metotları: `insert_row(path, values)` ve `delete_row(path, key)`.
- Postgres ve SQL Server: identity ve computed kolonlar hariç `INSERT`. `DELETE` transaction içinde çalışacak ve tam 1 satır silindiğini doğrulayacak.
- MongoDB: `insert_one` ve `_id` adaylarıyla `delete_one`.
- Redis: list için `RPUSH` ve LSET+LREM ile silme; hash için `HSETNX`/`HDEL`; set için `SADD`/`SREM` (set'e edit target eklenecek); zset için `ZADD NX`/`ZREM`; stream için `XADD *`/`XDEL`.
- ClickHouse: desteklenmeyecek.
- UI: "Add Row" alanları grid'in altında açılacak. "Delete Row" seçili satıra uygulanacak, satır `visible_rows` üzerinden eşlenecek ve onay sorulacak. İşlemden sonra önizleme yenilenecek.

### 8. MongoDB JSON görünümü ve şablonlar
- Seçili satır, yanda salt-okunur bir JSON editöründe gösterilecek. Editörün katlama özelliği ağaç görünümü işlevi görür. MongoDB için `ResultSet`'e ham belge JSON'u eklenecek, diğer motorlarda satır kolonlardan nesneye çevrilecek. Tetikleyici `TableViewEvent::SelectionChanged`.
- Motor başına şablon menüsü olacak (yeni `templates.rs`). MongoDB için: find, aggregate, count, distinct, insert, update, delete, createIndexes. SQL ve Redis için de temel kalıplar eklenecek.

### 9. Redis
- Anahtarların sağ tık menüsüne: Set TTL…, Persist, Rename…, Delete Key (onaylı).
- Veritabanı ve klasör düğümlerine: Find Keys… (desen), sonucu `Driver::search(path, pattern)` ile yeni sekmede göstermek.
- `password_prompt` genel bir satır içi isteme (`InlinePrompt { kind, editor }`) dönüştürülecek.

### 10. Şema karşılaştırma
- Şemaya sağ tıklayıp "Select for Compare", ardından başka bir şemada (başka bir bağlantıda da olabilir, aynı motor) "Compare with …".
- `Driver::schema_snapshot(path)` tabloları, kolonları, tipleri, null olabilirliği ve varsayılanları döndürecek. Fark hesaplanıp motora uygun `ALTER`, `CREATE` ve `DROP` script'i hedef bağlantıda yeni bir sekmede açılacak, ama **çalıştırılmayacak**.
- MongoDB ve Redis şemasız oldukları için kapsam dışı.

### 11. SSH tüneli ve TLS
- URL parametreleri: `?ssh=user@bastion:22&tls=require|verify|disable` (ayrıştırıcı şu an `?` kısmını yok sayıyor). `ConnectionConfig`'e `ssh` ve `tls` alanları eklenecek, `connection_url` bunları geri yazacak.
- Tünel sistemin `ssh` komutuyla kurulacak: `ssh -N -L 127.0.0.1:<boş port>:host:port -o ExitOnForwardFailure=yes -o BatchMode=yes`. Port açılana kadar beklenecek. Sürücü, drop edildiğinde süreci öldüren bir `TunneledDriver` ile sarılacak. BatchMode olduğu için yalnızca anahtar ya da agent ile giriş çalışır.
- TLS: Postgres'te `PgSslMode`. SQL Server şu an hep şifreli ve sertifikaya güveniyor; `verify` seçeneğinde `trust_cert` kaldırılacak. MongoDB'de `TlsOptions`. ClickHouse'ta https. Redis için crate'e TLS özelliği eklenmesi gerekiyor.

### 12. Ajan entegrasyonu
- Araştırılmadı. Zed'in yerel ajanına araç eklemenin upstream koda dokunmadan yapılıp yapılamayacağına bakılmalı; dokunmak rebase'te çatışma demek. Alternatif olarak salt-okunur bir MCP sunucusu düşünülebilir.

## Notlar
- Canlı testler, `C:\Users\JosephShall\tools\quarry` altındaki docker-compose konteynerlerini ve seed verisini kullanır. `*_preview_pages` testleri kendi tablolarını kurup siliyor, seed'e bağlı değil. `postgres_preview_does_not_join_another_scan` varsayılan shared_buffers'ı (128MB) varsayıyor; daha büyükse bunu söyleyerek düşüyor.
- Aynı çalışma ağacında başka oturumlar da çalışıyor (`git_ui`, `acp_thread` vb.). Commit'lere yalnızca `crates/quarry` dosyaları alınmalı. Tek istisna: quarry'ye bağımlılık eklemek `Cargo.lock`'taki quarry bloğunu da değiştiriyor.
- Load More'un bilinen sınırları:
  - Sayfa eklenince grid en başa dönüyor; yeni kolon gelirse elle ayarlanmış kolon genişlikleri de sıfırlanıyor. `TableView::set_contents` liste durumunu sıfırlıyor ve kaydırma konumunu koruyacak bir API yok; `tabular_data_preview`'a dokunulmadı.
  - Sayfalar OFFSET ile, sunucunun doğal sırasında okunuyor. Sayfalar arasında tablo değişirse satırlar kayabilir, tekrar edebilir ya da atlanabilir. Bu sekmede yapılan bir düzenleme de buna dahil: Postgres'te satırı başka bir sayfaya taşıyan bir UPDATE, Redis sorted set'te skor değişikliği.
  - SQL Server'da ilk sayfanın TOP'u ile sonrakilerin OFFSET ... FETCH'inin aynı sırayı verdiği doğrulanmadı.
