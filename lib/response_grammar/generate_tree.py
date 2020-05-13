import sys
from antlr4 import *
from antlr4.tree.Trees import Trees
from ResponseLexer import ResponseLexer
from ResponseParser import ResponseParser

def main(argv):
    input_stream = FileStream(argv[1])
    lexer = ResponseLexer(input_stream)
    stream = CommonTokenStream(lexer)
    parser = ResponseParser(stream)
    tree = parser.response()
    print(Trees.toStringTree(tree, None, parser))


if __name__ == '__main__':
    main(sys.argv)
