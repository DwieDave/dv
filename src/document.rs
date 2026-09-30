//! One document type for the app: in memory, live (indexing) or streamed (D-14).

use std::borrow::Cow;
use std::ops::Range;

use crate::error::ParseErrorKind;
use crate::format::Format;
use crate::index::IndexError;
use crate::index::children::Child;
use crate::index::spill::SpillStore;
use crate::live_tree::LiveTree;
use crate::source::file::FileSource;
use crate::stream_tree::StreamTree;
use crate::tree::{Count, MemTree, NodeRef, Stats, TreeIndex};

/// Any loaded document.
#[derive(Debug)]
pub enum Document {
    Mem(MemTree),
    Live(LiveTree<FileSource>),
    /// Boxed: the finished index is much larger than the other variants.
    Stream(Box<StreamTree<FileSource, SpillStore>>),
}

impl Document {
    fn inner(&self) -> &dyn TreeIndex {
        match self {
            Self::Mem(tree) => tree,
            Self::Live(tree) => tree,
            Self::Stream(tree) => tree.as_ref(),
        }
    }
}

impl TreeIndex for Document {
    fn root(&self) -> Result<NodeRef, IndexError> {
        self.inner().root()
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        self.inner().child_count(node)
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        self.inner().children(node, range)
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        self.inner().child_containing(node, offset)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        self.inner().bytes(range)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        self.inner().value_end(node)
    }

    fn stats(&self) -> Stats {
        self.inner().stats()
    }

    fn format(&self) -> Format {
        self.inner().format()
    }

    fn is_alias(&self, node: NodeRef) -> bool {
        self.inner().is_alias(node)
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        self.inner().problem(node)
    }

    fn streamed(&self) -> bool {
        self.inner().streamed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;
    use crate::test_support::to_value;

    #[test]
    fn delegates_to_the_wrapped_tree() {
        let text = br#"{"a": [1, {"b": null}], "c": "x"}"#;
        let tree = MemTree::parse(MemSource::new(text.to_vec())).unwrap();
        let expected = to_value(&tree, tree.root().unwrap());
        let doc = Document::Mem(tree);
        assert_eq!(to_value(&doc, doc.root().unwrap()), expected);
        assert_eq!(
            (doc.format(), doc.stats().bytes),
            (Format::Json, text.len() as u64)
        );
    }

    #[test]
    fn only_file_backed_documents_are_streamed() {
        use std::io::Write;
        use std::sync::atomic::AtomicBool;

        use crate::load::{LoadEvent, StreamBudget, load_stream};
        let text = br#"{"a": [1, 2, 3]}"#;
        assert!(!Document::Mem(MemTree::parse(MemSource::new(text.to_vec())).unwrap()).streamed());
        let mut file = crate::temp::file().unwrap();
        file.write_all(text).unwrap();
        let mut docs = Vec::new();
        let sink = &mut |e| {
            if let LoadEvent::Live(doc) | LoadEvent::Loaded(Ok(doc)) = e {
                docs.push(doc);
            }
        };
        load_stream(
            &file,
            Format::Json,
            sink,
            &AtomicBool::new(false),
            StreamBudget::testing(),
        );
        assert!(matches!(
            &docs[..],
            [Document::Live(_), Document::Stream(_)]
        ));
        assert!(docs.iter().all(TreeIndex::streamed));
    }
}
