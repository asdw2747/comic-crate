# CBR test fixtures

These small archives are used only by the integration tests.

- `testfile.rar3.cbr`, `testfile.rar5.solid.cbr` — from
  [ssokolow/rar-test-files](https://github.com/ssokolow/rar-test-files),
  released under CC0. Each contains one tiny JPEG and one tiny PNG.
- `test_comic.cbr` — from
  [ciscoriordan/kindling](https://github.com/ciscoriordan/kindling)
  (`tests/fixtures/test_comic.cbr`, MIT). Contains three tiny PNG pages and a
  `ComicInfo.xml` with a title and writer.
- `test_comic.cbz` — generated from the fixture above by this project, used for
  reader tests.
- `invalid_images.cbr` — from
  [comictagger](https://github.com/comictagger/comictagger)
  (`testing/data/fake_cbr.cbr`, MIT). Image-named entries contain invalid
  two-byte payloads; used to verify strict all-or-nothing conversion failure.

They are deliberately tiny and contain no copyrighted artwork.
